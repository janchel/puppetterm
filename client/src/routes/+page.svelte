<script lang="ts">
  import { call, on, type UnlistenFn } from "$lib/backend";
  import { Terminal } from "xterm";
  import "xterm/css/xterm.css";
  import { FitAddon } from "@xterm/addon-fit";
  import { writeText as tauriWriteText, readText as tauriReadText } from "@tauri-apps/plugin-clipboard-manager";
  import { onMount, tick, untrack } from "svelte";

  const isTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

  type Tab = {
    id: number;
    host: string; // remote target ("" = local shell); updated when `ssh <target>` is detected
    cwd: string; // current working directory, parsed from the shell prompt ("" = unknown)
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
  // Current working directory of the active session, parsed from its prompt.
  let activeTabCwd = $derived(tabs.find((t) => t.id === activeTabId)?.cwd ?? "");

  // Pinned at send time: the terminal + host a chat task is acting on. Kept so
  // switching tabs mid-task can never redirect the AI to a different server.
  let chatTarget = $state<{ host: string; tabId: number } | null>(null);
  // The tool set + system prompt chosen for the current task, based on whether
  // the remote agent is installed (agent mode vs terminal-only mode). Assigned
  // in sendChat (the prompt/tool constants are defined further down).
  let chatTools = $state<any[]>([]);
  let chatPrompt = $state<string>("");

  // ---- agent install (in-terminal, approval-style: install the agent on the connected host) ---
  let installPrompt = $state<{ tabId: number; host: string; force?: boolean } | null>(null); // awaiting y/n in the terminal
  let installBusy = $state(false);
  let installTabId = $state<number | null>(null); // route install-output events here
  let agentChecked = $state<Set<string>>(new Set()); // hosts we've already hinted about
  // Known agent presence per host (true = installed). Populated by check_agent;
  // the AI's tool set + system prompt depend on this, so the model knows
  // whether it's in agent mode or terminal-only mode.
  let agentMap = $state<Record<string, boolean>>({});

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
  // How the bearer token is obtained: "key" (static) or "oauth" (browser login).
  let aiAuthMethod = $state("key");
  // OAuth client metadata (shown when auth_method === "oauth").
  let oauthAuthUrl = $state("");
  let oauthTokenUrl = $state("");
  let oauthClientId = $state("");
  let oauthClientSecret = $state("");
  let oauthScope = $state("");
  let oauthRedirectUri = $state("");
  let oauthFlow = $state("standard");
  let oauthHasSecret = $state(false);
  let aiOauthBusy = $state(false);
  let activeSettingsTab = $state<"api" | "oauth" | "models" | "general">("api");
  let modelsList = $state<string[]>([]);
  let modelsLoading = $state(false);
  let modelsError = $state<string | null>(null);
  let enabledModels = $state<string[]>([]);
  // Result of a "test connection" probe (before saving).
  let aiTest = $state<{ ok: boolean; msg: string } | null>(null);
  let aiTestBusy = $state(false);
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
      models: [],
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

  // OAuth provider presets — one click fills the endpoint + OAuth metadata so
  // the user only needs to register an OAuth app (client id/secret) and log in.
  // `flow` selects the backend exchange variant ("standard" or "openrouter").
  const AI_OAUTH_PRESETS: Record<
    string,
    { label: string; flow: string; baseUrl: string; model: string; authUrl: string; tokenUrl: string; scope: string }
  > = {
    "github-models": {
      label: "GitHub Models (OAuth)",
      flow: "standard",
      baseUrl: "https://models.inference.ai.azure.com/openai/v1",
      model: "gpt-4o",
      authUrl: "https://github.com/login/oauth/authorize",
      tokenUrl: "https://github.com/login/oauth/access_token",
      scope: "read:models",
    },
    openrouter: {
      label: "OpenRouter (OAuth)",
      flow: "openrouter",
      baseUrl: "https://openrouter.ai/api/v1",
      model: "",
      authUrl: "https://openrouter.ai/auth",
      tokenUrl: "https://openrouter.ai/api/v1/auth/keys",
      scope: "",
    },
    google: {
      label: "Google (Chrome account)",
      flow: "standard",
      baseUrl: "https://generativelanguage.googleapis.com/v1beta/openai/",
      model: "gemini-3.6-flash",
      authUrl: "https://accounts.google.com/o/oauth2/v2/auth",
      tokenUrl: "https://oauth2.googleapis.com/token",
      scope: "https://www.googleapis.com/auth/generative-language.retriever https://www.googleapis.com/auth/cloud-platform",
    },
  };

  // Models from the active provider's preset + fetched provider list.
  // Disable-all-by-default: once models have been fetched (modelsList non-empty),
  // only explicitly enabled models appear (plus the current aiModel).
  let allModels = $derived.by(() => {
    const set = new Set<string>();
    const preset = AI_PROVIDERS[aiProvider];
    if (preset) for (const m of preset.models) set.add(m);
    for (const m of modelsList) set.add(m);
    if (aiModel) set.add(aiModel);
    let arr = [...set];
    if (modelsList.length > 0) {
      const enabled = new Set(enabledModels);
      arr = arr.filter((m) => enabled.has(m) || m === aiModel);
      if (aiModel && !arr.includes(aiModel)) arr.push(aiModel);
      return arr.sort();
    }
    if (enabledModels.length > 0) {
      const enabled = new Set(enabledModels);
      arr = arr.filter((m) => enabled.has(m));
      if (aiModel && !enabled.has(aiModel)) arr.push(aiModel);
    }
    return arr.sort();
  });

  // Persist the enabled-models filter
  try {
    const raw = localStorage.getItem("pp.ai.enabledModels");
    if (raw) {
      const v = JSON.parse(raw);
      if (Array.isArray(v)) enabledModels = v;
    }
  } catch {}
  $effect(() => {
    try {
      // touch enabledModels to make this reactive
      const _ = enabledModels;
      localStorage.setItem("pp.ai.enabledModels", JSON.stringify(enabledModels));
    } catch {}
  });

  // Auto-fetch models when Settings opens and the provider is already authenticated
  $effect(() => {
    if (showSettings && aiHasKey && !modelsList.length && !modelsLoading && aiBaseUrl.trim()) {
      loadModels();
    }
  });

  function applyAiProvider(p: string) {
    aiProvider = p;
    const preset = AI_PROVIDERS[p];
    if (preset && preset.baseUrl) aiBaseUrl = preset.baseUrl;
    else if (p === "openai") aiBaseUrl = customBaseUrl; // restore the custom endpoint
    if (preset?.model) aiModel = preset.model;
  }

  /** Switch the authentication method. OAuth always targets an
   *  OpenAI-compatible endpoint, so the provider is forced to "openai" and the
   *  redirect URI is auto-filled from the current origin. */
  function setAiAuthMethod(m: string) {
    aiAuthMethod = m;
    if (m === "oauth") {
      if (aiProvider !== "openai") {
        aiProvider = "openai";
        aiBaseUrl = customBaseUrl;
      }
      if (!oauthRedirectUri.trim()) {
        try {
          oauthRedirectUri = `${location.origin}/oauth/callback`;
        } catch {
          oauthRedirectUri = "";
        }
      }
    }
  }

  /** Fill the OAuth fields from a provider preset (GitHub Models / OpenRouter). */
  function applyOauthPreset(key: string) {
    const p = AI_OAUTH_PRESETS[key];
    if (!p) return;
    aiProvider = "openai";
    aiBaseUrl = p.baseUrl;
    if (p.model) aiModel = p.model;
    oauthAuthUrl = p.authUrl;
    oauthTokenUrl = p.tokenUrl;
    oauthScope = p.scope;
    oauthFlow = p.flow;
    aiAuthMethod = "oauth";
    if (!oauthRedirectUri.trim()) {
      try {
        oauthRedirectUri = `${location.origin}/oauth/callback`;
      } catch {
        oauthRedirectUri = "";
      }
    }
  }

  /** Begin a browser-based OAuth login: save the metadata, open the provider's
   *  authorize URL, then poll until the token lands in the saved config. */
  async function startOauthLogin() {
    if (aiOauthBusy) return;
    aiOauthBusy = true;
    try {
      if (aiProvider === "openai") customBaseUrl = aiBaseUrl;
      await call("set_ai_config", {
        base_url: aiBaseUrl,
        model: aiModel,
        provider: aiProvider,
        api_key: aiKey,
        auth_method: "oauth",
        oauth: {
          auth_url: oauthAuthUrl,
          token_url: oauthTokenUrl,
          client_id: oauthClientId,
          client_secret: oauthClientSecret,
          scope: oauthScope,
          redirect_uri: oauthRedirectUri,
          flow: oauthFlow,
        },
      });
      const r = await call<any>("ai_oauth_begin");
      window.open(r.authorize_url, "puppetterm-oauth", "width=600,height=720");
      pushChat("ai", "(opened provider login — complete it in the popup, then return here)");
      const timer = setInterval(async () => {
        try {
          const v = await call<any>("get_ai_config");
          if (v.auth_method === "oauth" && v.has_api_key) {
            clearInterval(timer);
            aiHasKey = true;
            aiReady = true;
            pushChat("ai", "(AI logged in via OAuth — token stored, encrypted)");
            notify("OAuth login successful");
            loadModels().catch(() => {});
          }
        } catch {
          /* keep polling */
        }
      }, 1500);
      setTimeout(() => clearInterval(timer), 5 * 60 * 1000);
    } catch (e) {
      notify(`OAuth login failed: ${e}`, "err");
      pushChat("ai", `(OAuth login failed: ${e})`);
    } finally {
      aiOauthBusy = false;
    }
  }

  async function loadModels() {
    if (!aiHasKey) {
      modelsError = "Configure a provider and authenticate first.";
      return;
    }
    modelsLoading = true;
    modelsError = null;
    try {
      const r = await call<any>("list_ai_models");
      const list: string[] = r.models ?? [];
      modelsList = list;
      if (list.length === 0) modelsError = "No models returned.";
    } catch (e) {
      modelsError = String(e);
    } finally {
      modelsLoading = false;
    }
  }
  function toggleModel(m: string) {
    const s = new Set(enabledModels);
    if (s.has(m)) s.delete(m);
    else s.add(m);
    enabledModels = [...s];
  }

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
      base_url: aiBaseUrl,
      model: aiModel,
      provider: aiProvider,
    }).catch((e) => console.error("save model", e));
  }
  let chatBusy = $state(false);
  let aiThinking = $state(false);
  let chatText = $state("");
  // Chat history is persisted locally (see the $effect below) so a page reload
  // keeps the conversation; we restore it here on first load.
  // Chat sessions are kept per-host: one conversation per SSH target / tab, so
  // switching tabs switches the visible history. Each is stored under its own
  // localStorage key (pp.chat.<host>.session).
  function chatKey(host: string) {
    return `pp.chat.${host}.session`;
  }
  function chatHostOf(h: string | null | undefined) {
    return h && h.length ? h : "__local__";
  }
  function loadChatFor(host: string): { chatLog: any[]; history: any[] } {
    if (typeof localStorage === "undefined") return { chatLog: [], history: [] };
    let raw = localStorage.getItem(chatKey(host));
    // Back-compat: fall back to the old single global session if it matches this host.
    if (!raw) {
      const g = localStorage.getItem("pp.chat.session");
      if (g) {
        try {
          const v = JSON.parse(g);
          if (v && v.host === host && Array.isArray(v.chatLog)) raw = g;
        } catch {
          /* ignore */
        }
      }
    }
    if (raw) {
      try {
        const v = JSON.parse(raw);
        if (v && Array.isArray(v.chatLog)) {
          return {
            chatLog: v.chatLog,
            history: Array.isArray(v.history) ? v.history : [],
          };
        }
      } catch {
        /* corrupt — start fresh */
      }
    }
    return { chatLog: [], history: [] };
  }

  const _initialHost = chatHostOf(activeHost);
  const _initial = loadChatFor(_initialHost);
  let chats = $state<Record<string, { chatLog: any[]; history: any[] }>>({});
  // Seed the active host from storage (per-host) + keep one conversation per
  // SSH target / tab, so switching tabs switches the visible history.
  chats[_initialHost] = _initial;
  // The visible chat is always the active host's entry (derived, so it follows
  // the tab automatically). Writes go through helpers that route to the right
  // host — a pinned in-flight task streams into its OWN host's chat, not the
  // one currently on screen.
  let chatLog = $derived(chats[chatHostOf(activeHost)]?.chatLog ?? []);
  let history = $derived(chats[chatHostOf(activeHost)]?.history ?? []);
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
  // Live host resources (via the agent), shown in the "acting on <host>" bar.
  let liveMetrics = $state<{ cpu: number; mem: number; load: number } | null>(null);
  let metricsTimer: ReturnType<typeof setInterval> | null = null;
  let showActivity = $state(false);
  let expandedActivityId = $state<number | null>(null);
  // Full per-entry output, pulled on demand from the server (file-backed detail
  // store) when a row is expanded — kept out of the audit index and out of AI context.
  let activityDetails = $state<Record<number, any>>({});

  $effect(() => {
    localStorage.setItem("pp.autonomy", autonomy);
  });

  function chatEntry(host: string) {
    if (!chats[host]) {
      chats[host] = { chatLog: [], history: [{ role: "system", content: SYSTEM_PROMPT }] };
    }
    return chats[host];
  }
  function saveChat(host: string) {
    if (typeof localStorage === "undefined") return;
    const e = chats[host];
    if (!e) return;
    try {
      localStorage.setItem(
        chatKey(host),
        JSON.stringify({ chatLog: e.chatLog, history: e.history, host, savedAt: Date.now() }),
      );
    } catch {
      /* ignore quota / serialization errors */
    }
  }
  function appendChat(host: string, role: string, text: string) {
    const e = chatEntry(host);
    e.chatLog = [...e.chatLog, { role, text }];
    saveChat(host);
  }
  function setHistory(host: string, next: any[]) {
    const e = chatEntry(host);
    e.history = next;
    saveChat(host);
  }
  function getHistory(host: string) {
    return chats[host]?.history ?? [{ role: "system", content: SYSTEM_PROMPT }];
  }
  // Messages route to the pinned task's host when one is running, else the
  // active host (so streaming during a tab switch lands in the right chat).
  function pushChat(role: string, text: string) {
    const host = chatTarget?.host ? chatHostOf(chatTarget.host) : (activeHost ?? "__local__");
    appendChat(host, role, text);
  }

  // Persist the active conversation locally (per host) so a reload restores it.
  // Deep dependency on `chats` re-runs on every message; wrapped in try/catch
  // because large tool-output histories can exceed the quota.
  $effect(() => {
    if (typeof localStorage === "undefined") return;
    const host = chatHostOf(activeHost);
    const e = chats[host];
    if (!e) return;
    try {
      localStorage.setItem(
        chatKey(host),
        JSON.stringify({ chatLog: e.chatLog, history: e.history, host, savedAt: Date.now() }),
      );
    } catch {
      /* ignore quota / serialization errors */
    }
  });

  // Switching tabs changes the active host; the derived chatLog/history already
  // follow it. Just flush the PREVIOUS host's chat to storage on the way out
  // (it's only re-saved by the effect above while it's the active host).
  // `appliedHost` is a plain (non-reactive) guard so this runs only on a switch.
  let appliedHost: string | null = null;
  $effect(() => {
    const host = chatHostOf(activeHost);
    if (host === appliedHost) return;
    untrack(() => {
      if (appliedHost) saveChat(appliedHost);
    });
    appliedHost = host;
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

  // The `terminal` tool types a command into the user's LIVE pty (like a human)
  // and waits for the output — it's the universal fallback that works on ANY
  // host (key or password, no agent). It rides the real session, so in agent
  // mode it is a LAST RESORT for interactive commands the structured tools
  // can't cover (htop, vim, password prompts) — NOT for regular inspection,
  // which belongs to `run_command`/`config`/`log`/`snapshot`/`service`.
  const TERMINAL_TOOL = {
    type: "function",
    function: {
      name: "terminal",
      description:
        "Run a command by typing it into the user's LIVE, already-connected terminal (like a human) and wait for the output to settle. Works on ANY host — key or password — and needs no agent. ONLY for interactive commands the structured tools cannot handle (e.g. htop, vim, password prompts). For regular inspection and changes, prefer `run_command` (agent mode) or `read_terminal`/this tool (terminal mode). The user sees the command run live. Returns the terminal output after the command.",
      parameters: {
        type: "object",
        properties: { command: { type: "string", description: "the full command line to type and execute" } },
        required: ["command"],
      },
    },
  };

  // `read_terminal` reads what is physically on the user's screen (bounded by
  // xterm scrollback). It is ONLY offered in terminal mode, where there is no
  // agent to return structured results. In agent mode it would show just status
  // lines (silent mode) and make real results look truncated — so it is NOT in
  // AGENT_TOOLS. The agent tools return full output directly in the conversation.
  const READ_TERMINAL_TOOL = {
    type: "function",
    function: {
      name: "read_terminal",
      description:
        "Read what is physically on the user's terminal screen right now (bounded by scrollback). Use this ONLY to see what the user is looking at or an interactive session. This is NOT how you get command output or file contents — use `run_command`/`config`/`log` (agent mode) or the `terminal` tool (terminal mode) instead, which return full results directly.",
      parameters: { type: "object", properties: {} },
    },
  };

  // AGENT MODE: the agent is your eyes and hands on the remote host — every
  // tool runs ON THE MACHINE over the agent's own SSH connection and returns
  // full, audited results directly in the conversation. No terminal screen
  // scraping AND no typing into the live terminal: `run_command` is PRIMARY
  // (read files, run commands), `snapshot`, `service`, `log`, `config` cover
  // the rest. If the agent is unavailable, the app says so and guides you to
  // install it — it never falls back to typing commands into the user's pty.
  const AGENT_TOOLS = [
    { type: "function", function: { name: "run_command", description: "Run a command on the ACTIVE HOST through the installed puppetterm-agent over its dedicated SSH connection. This is your PRIMARY tool — your eyes and hands on the machine. Use it to inspect (read files with cat/sed/head/tail — full content returned; check state with ps/df/free/uptime/grep) and to change (install, configure, restart). Returns the FULL output and a clean exit code directly to you (audited).", parameters: { type: "object", properties: { cmd: { type: "string" }, dir: { type: "string" } }, required: ["cmd"] } } },
    { type: "function", function: { name: "snapshot", description: "System snapshot of the active host: CPU, memory, disk, uptime (via the installed agent).", parameters: { type: "object", properties: {} } } },
    { type: "function", function: { name: "service", description: "Control a systemd service on the active host (via the installed agent).", parameters: { type: "object", properties: { unit: { type: "string" }, op: { type: "string", enum: ["status", "is-active", "is-enabled", "start", "stop", "restart", "enable", "disable"] } }, required: ["unit", "op"] } } },
    { type: "function", function: { name: "log", description: "Tail a log file on the active host, allow-listed paths (via the installed agent).", parameters: { type: "object", properties: { path: { type: "string" }, lines: { type: "number" }, follow: { type: "boolean" } }, required: ["path"] } } },
    { type: "function", function: { name: "config", description: "Read or write a config file on the active host, allow-listed paths (via the installed agent).", parameters: { type: "object", properties: { path: { type: "string" }, op: { type: "string", enum: ["read", "write"] }, content: { type: "string" } }, required: ["path", "op"] } } },
    { type: "function", function: { name: "read", description: "Read a BOUNDED line-range of ANY file on the active host (via the installed agent) — for paging through large logs/configs WITHOUT dumping the whole file into context. Returns line-numbered output. Use `offset` (1-based starting line) and `limit` to page (e.g. read first 200 lines, then offset 201). Prefer `grep \"pattern\"` via run_command to find the relevant section FIRST, then `read` just that range. Default limit 200, max 5000.", parameters: { type: "object", properties: { path: { type: "string" }, offset: { type: "number" }, limit: { type: "number" } }, required: ["path"] } } },
  ];

  // TERMINAL MODE (agent not installed on the host): only tools that ride the
  // user's live terminal — works on any host, incl. password-only. read_terminal
  // IS useful here because there is no structured agent to return results.
  const TERMINAL_ONLY_TOOLS = [TERMINAL_TOOL, READ_TERMINAL_TOOL];

  /** True when the puppetterm-agent is known to be installed on `host`
   *  (checked once per host per session; defaults to false while unknown). */
  function hostHasAgent(host: string | null | undefined): boolean {
    if (!host) return false;
    return agentMap[host] === true;
  }

  /** The tools + system prompt to hand the model for a given host, depending
   *  on whether the remote agent is installed: agent mode exposes the
   *  structured tools (agentic), otherwise the AI only gets the terminal tools.
   *  `cwd` (the session's current directory, if known) is injected so the AI
   *  knows WHERE it's working — like opencode locking to a folder. */
  function chatConfigFor(host: string | null | undefined, cwd?: string | null) {
    const hasAgent = hostHasAgent(host);
    const tools = hasAgent ? AGENT_TOOLS : TERMINAL_ONLY_TOOLS;
    const base = hasAgent ? AGENT_SYSTEM_PROMPT : TERMINAL_SYSTEM_PROMPT;
    // History is a terminal app's working context, NOT ground truth. Explicitly
    // tell the model to re-query the live server rather than trust stale chat
    // or activity output — keeps it honest about current config and avoids
    // wasting tokens re-reading old logs.
    const historyCaveat =
      "\n\nHISTORY IS EPHEMERAL — the chat and Activity log are working context only, never the source of truth about the server. Do NOT treat prior tool output or chat history as the host's CURRENT state. Whenever you need current config or status, re-query the LIVE server with the tools (run_command / config / snapshot / service, or the terminal tool). This is a terminal app: trust the server's CURRENT state, not stale history.";
    const cwdLine = cwd
      ? `\n\nYou are currently working in the directory \`${cwd}\` on ${host || "this host"}. Prefer commands relative to that directory (or reference the full path) when the user asks about files/folders there.`
      : "";
    const prompt = base + historyCaveat + cwdLine;
    return { tools, prompt, hasAgent };
  }

  // Agent-aware system prompts: the AI is told which mode it's in so it uses
  // the right tools instead of guessing.
  const TERMINAL_SYSTEM_PROMPT =
    "You are puppetterm, an AI assistant inside a terminal app. You manage the ACTIVE host " +
    "using the provided tools.\n\n" +
    "ANSWER QUESTIONS FIRST. When the user asks a question, answer it directly from your own " +
    "knowledge in text BEFORE calling any tool. Only run a command when you genuinely need LIVE " +
    "system state (current disk/memory, a service's real status, today's logs) or when the user " +
    "asked you to take an action. For general-knowledge questions, just give the answer and DO " +
    "NOT run a command at all.\n\n" +
    "The puppetterm-agent is NOT installed on this host, so run commands with the `terminal` " +
    "tool: it types the command into the user's live terminal (already logged in) and returns " +
    "the output — it works on ANY host, including password-only ones. Use `read_terminal` to " +
    "see the current terminal screen — the live view of the active session, NOT the shell " +
    "history file. `read_terminal` is a SCREEN SNAPSHOT bounded by scrollback: to get a " +
    "file's FULL contents or a command's full output, run it with the `terminal` tool (e.g. " +
    "`cat /path`) rather than reading the screen.\n\n" +
    "Before running anything, explain in text what you'll run and why — the user sees your " +
    "explanation before the approval prompt. Large COMMAND OUTPUT is trimmed to a digest " +
    "(first/last lines + any error/warning lines) to save tokens, but FILE READS (`cat`, `sed`, " +
    "`head`, `tail` of a file) return the FULL content — so the AI can inspect config/compose " +
    "files completely. A file read result starts with `[file: N bytes — COMPLETE, full content " +
    "below]` and ends with `[end of file]`, and carries a structured `complete: true` field: " +
    "that means the ENTIRE file is included and NOTHING was cut — do NOT tell the user the " +
    "output was truncated, cut off, or needs re-reading when you see those markers or `complete: " +
    "true`. Only treat a result as truncated if `complete` is false or it explicitly says " +
    "`TRUNCATED` or `… N lines omitted …`. If output is trimmed, run a follow-up like `grep`, " +
    "`tail -n`, `head -n` or `wc -l` via the `terminal` tool to narrow it. For a large file, " +
    "first check its size with `wc -l <file>`, then read ONLY the ranges you need with " +
    "`sed -n 'START,ENDp' <file>` or `grep -n 'pattern' <file>` — don't re-read the whole " +
    "file." +
    "\n\n" +
    "State-changing actions are approved by the user before execution; you will be told if one " +
    "is rejected. Be concise and summarize tool results for the user.";

  const AGENT_SYSTEM_PROMPT =
    "You are puppetterm, an AI assistant inside a terminal app. You are the AGENT'S EYES AND " +
    "HANDS on the ACTIVE host: you inspect and change the remote machine through the chat, " +
    "using the structured tools that run ON that machine.\n\n" +
    "ANSWER QUESTIONS FIRST. When the user asks a question, answer it directly from your own " +
    "knowledge in text BEFORE calling any tool. Only run a command when you genuinely need LIVE " +
    "system state (current disk/memory, a service's real status, today's logs) or when the user " +
    "asked you to take an action. For general-knowledge questions, just give the answer and DO " +
    "NOT run a command at all.\n\n" +
    "The puppetterm-agent IS installed on this host and reachable over its own SSH connection, " +
    "so DO ALL YOUR WORK THROUGH THE AGENT TOOLS — never by reading the terminal screen. Your " +
    "tool results come back to you DIRECTLY in this conversation: `run_command` returns the full " +
    "output + a clean exit code, `snapshot` returns CPU/memory/disk/uptime, `service` returns " +
    "systemd state, `log` and `config` return full file contents. In agent mode the terminal " +
    "only shows one status line per action — it does NOT show command output — so reading the " +
    "terminal would tell you nothing and make real results look truncated. If you need a " +
    "command's output or a file's contents, GET IT FROM THE TOOLS, not from the screen.\n\n" +
    "PREFER `run_command` for inspecting and changing the machine (read files with " +
    "`cat`/`sed`/`head`/`tail` — full content returned; check state with `ps`/`df`/`free`/" +
    "`uptime`; grep logs; install and configure). Use `config` (read/write) and `log` (tail) " +
    "for their allow-listed paths, `snapshot` for a system overview, and `service` for systemd " +
    "units.\n\n" +
    "LARGE FILES / LOGS: `run_command` output is capped at ~24k words PER command, so NEVER `cat` a huge log or file (the tail is truncated and you'll miss most of it). Instead: `grep \"pattern\"` via run_command to find the relevant section, then use the `read` tool with `offset`/`limit` to page through that exact range (line-numbered). This keeps the conversation bounded and you only ever see what's relevant.\n\n" +
    "IMPORTANT — YOU NEVER TYPE INTO THE USER'S TERMINAL in agent mode. All your work goes " +
    "through the agent tools above; the results appear here in the chat, and the terminal only " +
    "shows a status line. If a tool returns that the puppetterm-agent is not available on the " +
    "host, do NOT try to work around it by typing into the terminal (you have no such tool) — " +
    "tell the user the agent isn't installed and that they can click \"Install agent\" in the AI " +
    "panel to enable agent mode on this host.\n\n" +
    "NOTE: `run_command` runs in the user's HOME directory on the remote host, NOT your shell's " +
    "current folder — always use ABSOLUTE paths (e.g. `cat /opt/docker/mcp-rag/docker-compose." +
    "yml`) when reading files with it.\n\n" +
    "READING FILES: `config` (read) and `log` return the FULL content, but `config` only works " +
    "for paths in its allow-list; if `config` is rejected for a path, read the file with " +
    "`run_command` using an ABSOLUTE path, e.g. `cat /opt/docker/mcp-rag/docker-compose.yml` " +
    "(cat/sed/head/tail file reads return full content too).\n\n" +
    "Before running anything, explain in text what you'll run and why — the user sees your " +
    "explanation before the approval prompt. Large COMMAND OUTPUT is trimmed to a digest " +
    "(first/last lines + any error/warning lines) to save tokens, but FILE READS (via `config` " +
    "read, `log`, or `cat`/`sed`/`head`/`tail` of a file) return the FULL content. A file read " +
    "result starts with `[file: N bytes — COMPLETE, full content below]` and ends with `[end of " +
    "file]`, and carries a structured `complete: true` field: the ENTIRE file is included and " +
    "NOTHING was cut — do NOT tell the user the output was truncated, cut off, or needs " +
    "re-reading when you see those markers or `complete: true`. Only treat a result as truncated " +
    "if `complete` is false or it explicitly says `TRUNCATED` or `… N lines omitted …`. If " +
    "output is trimmed, run a follow-up like `grep`, `tail -n`, `head -n` or `wc -l` via " +
    "`run_command` to narrow it. For a large file, first check its size with `wc -l <file>`, " +
    "then read ONLY the ranges you need with `sed -n 'START,ENDp' <file>` or `grep -n " +
    "'pattern' <file>` — don't re-read the whole file.\n\n" +
    "State-changing actions are approved by the user before execution; you will be told if one " +
    "is rejected. Be concise and summarize tool results for the user.";

  const SYSTEM_PROMPT = TERMINAL_SYSTEM_PROMPT; // default until a host is detected

  const TOOL_TO_ACTION: Record<string, string> = {
    run_command: "run",
    snapshot: "snapshot",
    service: "service",
    log: "log",
    config: "config",
    read: "read",
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

  /** Extract the target host from a line like `ssh -p 2222 user@host` or
   *  `ssh user@host -p 2222`. Returns the host WITH a `:port` suffix when a
   *  non-standard port is given, so the (server-side) SSH calls for agent
   *  install/run/check can pass `-p`. OpenSSH itself doesn't accept `host:port`,
   *  but the backend rewrites it. The port is scanned from anywhere in the line
   *  (it may appear before OR after the host). */
  function parseSshTarget(line: string): string | null {
    const m = line.trim().match(/^ssh(?:2)?\s+(.+)$/i);
    if (!m) return null;
    const tokens = m[1].trim().split(/\s+/).filter(Boolean);
    let i = 0;
    let port: string | null = null;
    let host: string | null = null;
    while (i < tokens.length) {
      const tok = tokens[i];
      // `-p 2222` / `-p2222` (and `-P` just in case) carry the port.
      if (tok === "-p" || tok === "-P") {
        port = tokens[i + 1] ?? null;
        i += 2;
        continue;
      }
      if ((tok.startsWith("-p") || tok.startsWith("-P")) && tok.length > 2) {
        port = tok.slice(2);
        i += 1;
        continue;
      }
      if (
        tok === "-i" || tok === "-l" || tok === "-o" || tok === "-J" || tok === "-W"
      ) {
        i += 2; // option + its value (we don't forward these to the app's ssh)
        continue;
      }
      if (tok.startsWith("-")) {
        i += 1;
        continue;
      }
      // First non-option token is the destination host. Don't break — keep
      // scanning so a `-p` that comes AFTER the host is still captured.
      if (!host) {
        host = tok.replace(/[\s\x00-\x1f\x7f]/g, "");
      }
      i += 1;
    }
    if (!host) return null;
    if (port && /^\d+$/.test(port)) host = `${host}:${port}`;
    return host;
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
    tabs = [...tabs, { id, host: host ?? "", cwd: "", sessionId: null, connecting: false, buf: "" }];
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
          const { host, force } = installPrompt;
          installPrompt = null;
          runInstall(id, host, force ?? false);
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
    // Safety net: re-scan the newly-activated tab for an ssh target. The
    // pty-output scan can miss a session established before the app noticed
    // (or a recalled command), so re-detecting on activation keeps the host +
    // agent-mode badge in sync with reality.
    maybeDetectSshFromBuffer(id);
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

  /** Dump the current conversation to a local Markdown file. This is the
   *  human-readable transcript (what's visible in the chat panel); the full
   *  raw session — including tool outputs — also lives in localStorage under
   *  `pp.chat.<host>.session` (per host) for reload-restore and JSON export. */
  function dumpChat() {
    if (typeof document === "undefined" || chatLog.length === 0) return;
    const host = activeHost ?? "local";
    const md: string[] = [
      "# PuppetTerm chat",
      "",
      `**Host:** ${host}`,
      `**Dumped:** ${new Date().toISOString()}`,
      "",
    ];
    for (const m of chatLog) {
      const who = m.role === "user" ? "🧑 You" : m.role === "ai" ? "🤖 AI" : m.role;
      md.push(`### ${who}`, "", m.text, "");
    }
    const blob = new Blob([md.join("\n")], { type: "text/markdown" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `puppetterm-chat-${host.replace(/[^a-zA-Z0-9._-]/g, "_")}-${Date.now()}.md`;
    document.body.appendChild(a);
    a.click();
    a.remove();
    URL.revokeObjectURL(url);
    notify("Chat dumped to a local .md file");
  }

  /** Dump the full raw session (chatLog + all tool messages/history) as JSON —
   *  useful for archival or re-import. */
  function dumpChatJson() {
    if (typeof document === "undefined") return;
    const payload = {
      host: activeHost ?? "local",
      dumpedAt: new Date().toISOString(),
      chatLog,
      history,
    };
    const blob = new Blob([JSON.stringify(payload, null, 2)], {
      type: "application/json",
    });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `puppetterm-chat-${activeHost?.replace(/[^a-zA-Z0-9._-]/g, "_") ?? "local"}-${Date.now()}.json`;
    document.body.appendChild(a);
    a.click();
    a.remove();
    URL.revokeObjectURL(url);
    notify("Full chat session dumped to a local .json file");
  }

  /** Auto-grow the chat textarea with its content, capped at the CSS max-height
   *  (beyond which it scrolls). Set height to 'auto' first so scrollHeight
   *  reflects the natural (unconstrained) content height each time. */
  function autoGrowInput(el: HTMLTextAreaElement) {
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 140)}px`;
  }

  /** Start a fresh conversation: reset history to just the system prompt and
   *  clear the visible chat log. History is in-memory (not persisted) and is
   *  already bounded by compaction while a task runs. */
  function newChat() {
    if (chatBusy) return; // don't clear mid-task
    const h = chatHostOf(activeHost);
    setHistory(h, [{ role: "system", content: SYSTEM_PROMPT }]);
    const e = chatEntry(h);
    e.chatLog = [];
    saveChat(h);
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

  /** Short human-readable description of a structured agent action, shown as a
   *  single status line in the terminal during agent mode (silent mode: we show
   *  WHAT the AI ran, not the file contents / output bytes). */
  function describeAgentAction(name: string, args: Record<string, unknown>): string {
    switch (name) {
      case "run_command":
        return String(args.cmd ?? "").trim() || "run_command";
      case "snapshot":
        return "snapshot";
      case "service":
        return `service ${String(args.op ?? "status")} ${String(args.unit ?? "")}`.trim();
      case "log":
        return `log ${String(args.path ?? "")}${args.lines ? ` (${String(args.lines)} lines)` : ""}`.trim();
      case "config":
        return `config ${String(args.op ?? "read")} ${String(args.path ?? "")}`.trim();
      default:
        return `${name} ${JSON.stringify(args)}`;
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
        base_url: aiBaseUrl,
        model: aiModel,
        provider: aiProvider,
        api_key: aiKey,
        auth_method: aiAuthMethod,
        oauth: {
          auth_url: oauthAuthUrl,
          token_url: oauthTokenUrl,
          client_id: oauthClientId,
          client_secret: oauthClientSecret,
          scope: oauthScope,
          redirect_uri: oauthRedirectUri,
          flow: oauthFlow,
        },
      });
      aiKey = "";
      const v = await call<any>("get_ai_config");
      aiBaseUrl = v.base_url;
      aiModel = v.model;
      aiProvider = v.provider ?? "openai";
      aiHasKey = v.has_api_key;
        aiAuthMethod = v.auth_method ?? "key";
        if (v.oauth) {
          oauthAuthUrl = v.oauth.auth_url ?? "";
          oauthTokenUrl = v.oauth.token_url ?? "";
          oauthClientId = v.oauth.client_id ?? "";
          oauthScope = v.oauth.scope ?? "";
          oauthRedirectUri = v.oauth.redirect_uri ?? "";
          oauthFlow = v.oauth.flow ?? "standard";
          oauthHasSecret = !!v.oauth.has_client_secret;
        }
        aiReady = true;
        if (aiProvider === "openai") customBaseUrl = v.base_url || customBaseUrl;
      pushChat("ai", "(AI settings saved)");
      notify(`AI settings saved — ${AI_PROVIDERS[aiProvider]?.label ?? "Custom"} · ${aiModel}`);
      // Auto-discover models for Custom / OAuth so the dropdown isn't stale/empty
      try {
        await loadModels();
        if (!aiModel && modelsList.length) aiModel = modelsList[0];
      } catch {}
      // keep the newly discovered model persisted
      if (aiModel && modelsList.includes(aiModel)) {
        try {
          await call("set_ai_config", { base_url: aiBaseUrl, model: aiModel, provider: aiProvider });
        } catch {}
      }
    } catch (e) {
      pushChat("ai", `(failed to save AI settings: ${e})`);
      notify(`Failed to save AI settings: ${e}`, "err");
    }
  }

  /** Probe the endpoint/model/key with a tiny completion before saving. */
  async function testAiConfig() {
    aiTestBusy = true;
    aiTest = null;
    try {
      const r = await call<any>("test_ai_config", {
        base_url: aiBaseUrl,
        model: aiModel,
        provider: aiProvider,
        api_key: aiKey,
      });
      if (r && r.ok) aiTest = { ok: true, msg: r.summary };
      else aiTest = { ok: false, msg: r?.error ?? "connection test failed" };
    } catch (e) {
      aiTest = { ok: false, msg: String(e) };
    } finally {
      aiTestBusy = false;
    }
  }

  /** Delete the saved AI provider config entirely. */
  async function deleteAiConfig() {
    if (!confirm("Delete the saved AI provider config? The endpoint/key will be cleared.")) return;
    try {
      await call("delete_ai_config");
      aiBaseUrl = "";
      customBaseUrl = "";
      aiModel = "";
      aiKey = "";
      aiAuthMethod = "key";
      oauthAuthUrl = "";
      oauthTokenUrl = "";
      oauthClientId = "";
      oauthClientSecret = "";
      oauthScope = "";
      oauthRedirectUri = "";
      oauthFlow = "standard";
      oauthHasSecret = false;
      aiHasKey = false;
      aiReady = false;
      aiTest = null;
      notify("AI provider config deleted");
    } catch (e) {
      notify(`Failed to delete AI config: ${e}`, "err");
    }
  }

  /** Save everything from the Settings modal and close it. */
  async function saveSettings() {
    await saveAiConfig();
    // stay open so the user can switch tabs (Models etc.) — close only via Cancel / × / Esc
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
    // Keep agent presence fresh so we pick the right mode. The in-memory
    // agentMap is only set by the in-UI install flow or the terminal-buffer
    // auto-detect; if the agent was installed out-of-band (e.g. via the API)
    // that flag stays false and we'd wrongly fall back to terminal mode — the
    // AI would type into the terminal instead of using the agent tools. Re-check
    // the host once per chat so agent mode activates automatically.
    if (target.host) {
      try {
        const ok = await call<boolean>("check_agent", { host: target.host });
        agentMap[target.host] = ok;
        agentChecked.add(target.host);
      } catch {
        /* leave the current state as-is */
      }
    }
    // Agent-aware: choose the tool set + system prompt based on whether the
    // remote agent is installed, so the AI knows if it's in agentic mode. The
    // tab's current working directory (parsed from the prompt) is also passed
    // so the AI knows where it's working.
    const tab = target.tabId >= 0 ? tabs.find((x) => x.id === target.tabId) : null;
    const cfg = chatConfigFor(target.host, tab?.cwd || null);
    chatTools = cfg.tools;
    chatPrompt = cfg.prompt;
    abortRequested = false;
    chatText = "";
    pushChat("user", text);
    pushChat(
      "ai",
      `(acting on ${target.host || "the local terminal"} — ${cfg.hasAgent ? "agent mode" : "terminal mode"})`,
    );
    // Keep the system prompt in the conversation in sync with the current host
    // (agent vs terminal mode), so a host switch mid-conversation re-frames it.
    // All writes route to the target host's chat (see pushChat / setHistory).
    const th = chatHostOf(target.host);
    const userMsg = { role: "user", content: text };
    setHistory(th, [
      { role: "system", content: cfg.prompt },
      ...getHistory(th).filter((m) => m.role !== "system"),
      userMsg,
    ]);
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
      activity = await call<any[]>("audit_recent", { limit: 100 });
    } catch {
      /* audit may be unavailable in the browser mock — leave empty */
    }
  }

  /** Poll the agent's lightweight `metrics` action for the active host and
   *  update the live resource readout. Cheap: the agent samples /proc directly
   *  and returns just CPU%/MEM%/load1 (no snapshot payload). */
  async function pollMetrics() {
    const host = activeHost;
    if (!host || !hostHasAgent(host)) {
      liveMetrics = null;
      return;
    }
    try {
      const res = await call<any>("run_agent_action", {
        host,
        request: JSON.stringify({ action: "metrics", params: {} }),
        approved: true,
      });
      const ev = (res?.events ?? []).find((e: any) => e?.type === "result");
      const s = ev?.structured;
      if (s && typeof s.cpu_percent === "number") {
        liveMetrics = { cpu: s.cpu_percent, mem: s.mem_percent, load: s.load1 };
      } else {
        liveMetrics = null;
      }
    } catch {
      liveMetrics = null;
    }
  }

  // Keep the live resource readout fresh while an agent is active on the host.
  // Re-runs when the active host or its agent-presence flips; cleans up the
  // timer on change/unmount so we never poll a host without an agent.
  $effect(() => {
    const host = activeHost;
    const has = host ? hostHasAgent(host) : false;
    if (!has || !host) {
      liveMetrics = null;
      return;
    }
    pollMetrics();
    const t = setInterval(pollMetrics, 3000);
    return () => clearInterval(t);
  });

  function toggleActivity(id: number) {
    if (expandedActivityId === id) {
      expandedActivityId = null;
      return;
    }
    expandedActivityId = id;
    loadActivityDetail(id);
  }

  /** Pull the full output for an audit entry (stored server-side in a file, not
   *  in the SQLite index). Best-effort; never blocks the UI. */
  async function loadActivityDetail(id: number) {
    if (activityDetails[id] !== undefined) return; // already fetched (or failed)
    activityDetails = { ...activityDetails, [id]: "loading" }; // sentinel
    try {
      const d = await call<any>("audit_detail", { id: String(id) });
      activityDetails = { ...activityDetails, [id]: d?.detail ?? null };
    } catch {
      activityDetails = { ...activityDetails, [id]: null };
    }
  }

  /** Human-readable one-liner for an audit row (action + the key params). */
  function describeAction(a: any): string {
    let p: Record<string, unknown> = {};
    try {
      if (a.params) p = JSON.parse(a.params);
    } catch {
      /* keep empty */
    }
    const cmd = typeof p.cmd === "string" ? p.cmd : "";
    const unit = typeof p.unit === "string" ? p.unit : "";
    const op = typeof p.op === "string" ? p.op : "";
    const path = typeof p.path === "string" ? p.path : "";
    if (a.action === "run_command" && cmd) return `run: ${cmd}`;
    if (a.action === "service" && unit) return `service ${op || ""} ${unit}`.trim();
    if (a.action === "log" && path) return `log ${path}`;
    if (a.action === "config" && path) return `config ${op || ""} ${path}`.trim();
    if (a.action === "snapshot") return "snapshot";
    return a.action;
  }

  /** Pretty-print the raw params JSON for the detail view. */
  function prettyParams(raw: string | null | undefined): string {
    if (!raw) return "";
    try {
      return JSON.stringify(JSON.parse(raw), null, 2);
    } catch {
      return raw;
    }
  }

  // ---- agent install (in-terminal, approval-style) --------------------------------
  /** Ask in the terminal: install the agent on the active host? [y/N] */
  function promptInstall() {
    promptAgentAction("Install", "install");
  }

  /** Ask in the terminal: update (reinstall) the agent on the active host? */
  function promptUpdateAgent() {
    promptAgentAction("Update", "update");
  }

  function promptAgentAction(verb: string, mode: "install" | "update") {
    const host = activeHost;
    const tabId = activeTabId;
    const term = activeTerm();
    if (!host || !tabId || !term || installBusy) return;
    installPrompt = { tabId, host, force: mode === "update" };
    term.write(
      `\r\n\x1b[33m[puppetterm]\x1b[0m ${verb} puppetterm-agent on \x1b[1m${host}\x1b[0m ` +
        `(reuses your SSH connection)? [y/N] `,
    );
  }

  /** Stream the install into the terminal for the given tab. */
  async function runInstall(id: number, host: string, force = false) {
    const term = termByTab.get(id)?.term;
    installTabId = id;
    installBusy = true;
    term?.write(
      `\r\n\x1b[35m[puppetterm install] ${force ? "updating" : "starting on"} ${host}…\x1b[0m\r\n`,
    );
    try {
      const res = await call<any>("install_agent_on_host", { host, force });
      term?.write(
        `\r\n\x1b[32m[puppetterm install] ${res?.already && !force ? "already present —" : "done —"} ` +
          `${res?.mode ?? "user"} agent at ${res?.agent_path ?? "~/.puppetterm/bin/puppetterm-agent"}` +
          `\x1b[0m\r\n`,
      );
      // The agent is installed now — clear the "not detected" hint state and
      // re-verify so the UI reflects reality (and stops offering to install).
      // IMPORTANT: also record the result in agentMap so the badge AND the
      // AI's mode flip to agent mode immediately. Before this, runInstall
      // verified the agent and printed "agent detected ✓" but never updated
      // agentMap — so after reinstalling an already-installed agent the app
      // stayed stuck showing "terminal mode" (agentMap was stale from an
      // earlier failed action) and the AI kept using terminal tools.
      agentChecked.delete(host);
      try {
        const ok = await call<boolean>("check_agent", { host });
        agentMap[host] = ok;
        if (ok) {
          term?.write(`\r\n\x1b[32m[puppetterm] agent detected on ${host} ✓\x1b[0m\r\n`);
        }
      } catch {
        /* best-effort re-check */
      }
      loadActivity();
    } catch (e) {
      term?.write(`\r\n\x1b[31m[puppetterm install] failed: ${e}\x1b[0m\r\n`);
    } finally {
      installBusy = false;
      installTabId = null;
    }
  }

  /** After ssh detection, quietly check whether the agent is present; if not,
   *  print a one-time hint offering to install it. Records the result in
   *  agentMap so the AI knows which mode it's in (agent vs terminal-only). */
  async function checkAndHintAgent(id: number, host: string) {
    if (agentChecked.has(host)) return;
    agentChecked.add(host);
    try {
      const ok = await call<boolean>("check_agent", { host });
      agentMap[host] = ok;
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
      // Only accept plausible hosts: alphanumerics plus @ . : - . A wrapped
      // prose line (e.g. the install banner "…your SSH connection)? [y/N]")
      // can START with "ssh " and would otherwise parse as the garbage host
      // "connection)?" — which then fails check_agent and prints a bogus
      // "agent not detected on connection)?" hint.
      if (t && /^[A-Za-z0-9@._:\-]+$/.test(t)) target = t;
    }
    return target;
  }

  /** Extract the current working directory from a shell prompt line, e.g.
   *  `isr@svr-dev5:/opt/docker/mcp-rag$` → `/opt/docker/mcp-rag`, or
   *  `devops@box:~/proj$` → `~/proj`. Returns null when the prompt doesn't
   *  carry a directory (custom/unusual PS1). This is what lets the AI know
   *  WHERE it's working, like opencode locking to a folder. */
  function cwdFromPrompt(line: string): string | null {
    // user@host:<dir>$ or user@host:<dir># — dir is between the last ':' and
    // the trailing prompt char, may contain '~' for home.
    const m = line.match(/[^@\s]+@[^:\s]+:(\S*)[$#>]\s*$/);
    if (!m) return null;
    const dir = m[1].trim();
    // The captured group can include the dir plus trailing cruft from a busy
    // PS1 (git branch etc.) — keep only a clean path-ish token.
    const clean = dir.replace(/[()\[\]{}]|\(.*\)$/, "").trim();
    return clean && (clean.startsWith("/") || clean.startsWith("~")) ? clean : null;
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
    // Scan a generous window: a recalled `ssh …` can sit many lines up in the
    // buffer behind a long MOTD/scrollback (especially after history recall,
    // where the command never passes through onData).
    const lines = terminalText(term, 200).split("\n");
    // The shell prompt is the last non-empty line (a trailing blank line can
    // follow the prompt, e.g. after a redraw — skip it before gating).
    let li = lines.length - 1;
    while (li >= 0 && lines[li].trim() === "") li--;
    const last = (lines[li] ?? "").trim();
    if (!/[\$#>] ?$/.test(last)) return; // only when at a shell prompt
    // Track the working directory so the AI knows WHERE it's working (the
    // prompt carries it, e.g. `user@host:/opt/docker/mcp-rag$`).
    const dir = cwdFromPrompt(last);
    if (dir) t.cwd = dir;
    // If the remote session has ended (e.g. the user typed `exit`, or ssh
    // dropped), OpenSSH prints "Connection to <host> closed." — forget the
    // host so the status dot turns grey and the AI no longer targets it.
    // (The pty itself stays alive — it's the local shell — so pty-exit never
    // fires here; this is the reliable signal.)
    if (t.host) {
      const recent = lines.slice(Math.max(0, li - 12), li + 1).join("\n");
      if (/Connection (?:to .+? )?(?:closed|reset)|closed by remote host|Connection reset/i.test(recent)) {
        agentChecked.delete(t.host);
        t.host = "";
        return;
      }
    }
    // A host may already be known WITHOUT an `ssh` line in the buffer — e.g.
    // when connected directly via the host menu (`openTab(host)`), which starts
    // the session over SSH without echoing the command into the pty. In that
    // case we MUST still probe for the agent, or the app would stay stuck in
    // terminal mode even though the agent is installed.
    if (t.host) {
      checkAndHintAgent(id, t.host); // idempotent per host per session
      return;
    }
    // Otherwise discover an ssh target from the buffer (a command typed or
    // recalled into the live terminal).
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
      // Generous for legitimate multi-step work (reading a large file in
      // sections, several edits). The guard is only a safety net against
      // runaway loops — real loops are caught by the repeat check below
      // instead of just a raw step count, so 25 was cutting off genuine tasks
      // like "read server.py in sections" mid-way.
      const MAX_STEPS = 60;
      let lastSig: string | null = null;
      let repeatCount = 0;
      // All history writes/reads in this loop target the pinned task's host, so
      // a tab switch mid-task streams into the correct host's chat.
      const th = chatHostOf(chatTarget?.host ?? "");
      while (guard++ < MAX_STEPS) {
        if (abortRequested) {
          pushChat("ai", "(aborted by user)");
          return;
        }
        setHistory(th, compactHistory(getHistory(th)));
        aiThinking = true;
        let resp: any;
        try {
          resp = await call<any>("ai_chat", { messages: getHistory(th), tools: chatTools });
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
          setHistory(th, [
            ...getHistory(th),
            { role: "assistant", content: msg.content ?? null, tool_calls: msg.tool_calls },
          ]);
          for (const tc of msg.tool_calls) {
            // Loop guard: repeating the EXACT same tool call 3 times in a row
            // means the model is stuck, not making progress — stop early
            // instead of burning all MAX_STEPS.
            const sig = `${tc.function.name}:${tc.function.arguments}`;
            if (sig === lastSig) {
              repeatCount++;
            } else {
              lastSig = sig;
              repeatCount = 1;
            }
            if (repeatCount >= 3) {
              pushChat(
                "ai",
                "(stopped — the AI repeated the same action 3× in a row and appears stuck. Try rephrasing, or ask it to be more specific.)",
              );
              return;
            }
            const ok = await requestApproval(tc, explain || undefined);
            const content = ok
              ? JSON.stringify(await executeTool(tc))
              : JSON.stringify({ status: "rejected", reason: "user rejected the action" });
            setHistory(th, [...getHistory(th), { role: "tool", tool_call_id: tc.id, content }]);
          }
          continue;
        }
        const text = msg.content ?? "(done)";
        pushChat("ai", text);
        setHistory(th, [...getHistory(th), { role: "assistant", content: text }]);
        return;
      }
      pushChat(
        "ai",
        "(stopped after too many tool steps — the task may be too large for one go; try breaking it into smaller steps)",
      );
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

  /** Read the buffer from a given line to the end (used to capture everything
   *  from where the AI's command started, so long outputs aren't cut to the
   *  last few lines). Clamps to a max number of lines. */
  function terminalTextFrom(term: Terminal, startLine: number, maxLines = 600): string {
    const buf = term.buffer.active;
    const total = buf.length;
    const start = Math.max(startLine, total - maxLines);
    const lines: string[] = [];
    for (let y = start; y < total; y++) {
      lines.push(buf.getLine(y)?.translateToString(true) ?? "");
    }
    return lines.join("\n");
  }

  /** Lines worth surfacing when terminal output is trimmed — error / warning /
   *  failure-ish lines that usually matter for log questions. */
  const ERRORISH_LINE =
    /(?:error|errno|failed|failure|fatal|exception|traceback|panic|segfault|denied|timed?\s?out|refused|critical|warn(?:ing)?|out of memory|no space left)/i;

  /** Build a compact digest of large terminal output instead of sending the
   *  whole buffer (every char costs tokens). Small output passes through as-is;
   *  large output becomes: first ~25 lines + "… N omitted …" + last ~80 lines,
   *  plus any error/warning-ish lines found (deduped, capped). Returns the
   *  text plus whether it was trimmed and the true total line count.
   *
   *  `mode`:
   *   - "output" (default): command output → head/tail digest (ps, df, logs…)
   *   - "read": file contents → keep the FULL file, only capped at a much
   *     higher limit. When the AI reads a config/compose file it needs the
   *     whole thing, not just the first/last lines. */
  /** Real UTF-8 byte length of a string. The file on disk is bytes, but JS
   *  `.length` counts UTF-16 units — for non-ASCII content that under-reports
   *  (e.g. a 4929-byte compose file with multi-byte chars shows as 4639), so
   *  `[file: N bytes]` mismatches `ls -la` and the AI thinks the file was
   *  trimmed. Use this everywhere a byte count is shown to the model. */
  function utf8Bytes(s: string): number {
    return new TextEncoder().encode(s).length;
  }

  /** Index of the LAST buffer line whose visible text starts with `prefix`, or
   *  -1. Used to locate the AI-command banner so output is read from there —
   *  robust even when the scrollback is FULL: once the cap is hit,
   *  buffer.active.length stops growing (old lines scroll off the top), so an
   *  absolute start-line captured earlier goes stale and the capture returns
   *  0 bytes even though the output is on screen. */
  function lastLineStartingWith(term: Terminal, prefix: string): number {
    const buf = term.buffer.active;
    for (let y = buf.length - 1; y >= 0; y--) {
      if ((buf.getLine(y)?.translateToString(true) ?? "").startsWith(prefix)) return y;
    }
    return -1;
  }

  function buildOutputDigest(raw: string, capChars = 8000, mode: "output" | "read" = "output") {
    if (mode === "read") {
      const readCap = 100000; // full file up to ~100k bytes — a large compose/env/config set fits
      const bytes = utf8Bytes(raw);
      const totalLines = raw.split("\n").length;
      if (bytes <= readCap) {
        // Explicitly tell the model this is the COMPLETE file — otherwise a
        // long config can look "cut off" and the AI wastes turns re-reading it.
        // The closing [end of file] marker makes completeness unambiguous, and
        // the structured `complete: true` field is a machine-checkable signal
        // even models that skim the prose marker can trust.
        return {
          text: `[file: ${bytes} bytes, ${totalLines} lines — COMPLETE, full content below; the ENTIRE file is here, NOTHING was cut]\n${raw}\n[end of file]`,
          truncated: false,
          lines: totalLines,
          complete: true,
        };
      }
      return {
        text: `[file: ${bytes} bytes — TRUNCATED at ${readCap} bytes — content is INCOMPLETE]\n` + raw.slice(0, readCap) + "\n[end of truncated output]",
        truncated: true,
        lines: totalLines,
        complete: false,
      };
    }
    const lines = raw.split("\n");
    const total = lines.length;
    if (raw.length <= capChars) {
      return { text: raw, truncated: false, lines: total };
    }
    const head = lines.slice(0, 25).join("\n");
    const tail = lines.slice(-80).join("\n");
    const omitted = Math.max(0, total - 25 - 80);
    const hits = [...new Set(lines.filter((l) => ERRORISH_LINE.test(l)))].slice(0, 20);
    let digest = `[output: ${total} lines — trimmed to first 25 + last 80]\n`;
    digest += head;
    if (omitted > 0) digest += `\n… ${omitted} lines omitted …\n`;
    digest += tail;
    if (hits.length > 0) {
      digest += `\n[matched error/warning lines (${hits.length}):]\n` + hits.join("\n");
    }
    return { text: digest.slice(0, capChars + 2000), truncated: true, lines: total };
  }

  /** True when a `terminal`/`run_command` looks like it's READING a file's
   *  contents (so we return the full file instead of the head/tail digest).
   *  Matches common file-viewing invocations — plain `cat file`, numbered
   *  reads (`cat -n file`, `nl file`, `sed -n '1,50p' file`), `head`/`tail`
   *  of a file. Also accepts compound commands that END in a file read, e.g.
   *  `cd /dir && cat file` or `ls -la /dir; cat file` — the AI often chains
   *  an ls with a cat. Pipelines are treated as generic command output. */
  function isFileReadCommand(cmd: string): boolean {
    const c = cmd.trim().replace(/^sudo\s+/, "");
    // Take the LAST `;`/`&&`/`||` segment (chained like `ls; cat file` or
    // `cd dir && cat file`), then reject pipelines within that segment.
    const lastSeg = (c.split(/[;&]|\|\||&&/).filter(Boolean).pop() ?? c).trim();
    if (/[|]/.test(lastSeg)) return false; // pipelines → generic output
    const m = lastSeg.match(/^(\S+)\s+(.+)$/);
    if (!m) return false;
    const tool = m[1];
    const target = m[2]?.trim() ?? "";
    // The final (or only) argument must look like a file path, not a flag.
    const args = target.split(/\s+/);
    const lastArg = args[args.length - 1] ?? "";
    const looksLikePath = (a: string) => a.length > 0 && !a.startsWith("-") && /[.\/]/.test(a);
    if (["cat", "nl", "more", "less", "od", "xxd"].includes(tool)) {
      return args.some(looksLikePath);
    }
    if (tool === "sed") {
      // sed [-n] 'addr' file — file is the last non-flag arg
      return args.length >= 2 && looksLikePath(lastArg);
    }
    if (tool === "awk") {
      // awk 'program' file
      return args.length >= 2 && looksLikePath(lastArg);
    }
    if (tool === "head" || tool === "tail") {
      // head/tail [-n N] file — file is the last non-flag arg
      return args.some((t, i) => t !== "-n" && !/^\d+$/.test(t) && !t.startsWith("-") && looksLikePath(t));
    }
    return false;
  }

  /** Type a command into the LIVE terminal (like a human) and wait for the
   *  output to settle, then hand the result back to the AI. Works on any
   *  connection — key or password — because it rides the user's real pty. */
  async function runInTerminal(host: string | null, term: Terminal | null, cmd: string) {
    const tabId = chatTarget?.tabId ?? activeTabId;
    const t = tabId != null ? tabs.find((x) => x.id === tabId) : null;
    if (!term || !t || t.sessionId == null) {
      return { error: "no active terminal session to type into" };
    }
    // Write a unique banner, then type the command + Enter into the pty. We do
    // NOT rely on an absolute buffer index as the "start": once the terminal's
    // scrollback is full, buffer.active.length stops growing (old lines scroll
    // off the top), so an index captured here goes stale and the capture below
    // returns 0 bytes even though the output is on screen. Instead we locate
    // this banner line after the command and read from it.
    const markerPrefix = "[puppetterm] AI types: ";
    term.write(`\r\n\x1b[36m${markerPrefix}${cmd}\x1b[0m\r\n`);
    await call("write_ssh_input", { id: t.sessionId, data: cmd });
    await call("write_ssh_input", { id: t.sessionId, data: "\r" });
    // Wait for the command to finish. For file reads the reliable "done" signal
    // is the shell prompt returning AFTER the output (a large file over a slow
    // link can pause >1s between chunks, which a pure stable-window check would
    // mistake for completion). We require: the command banner has rendered AND
    // the last line looks like a prompt ($/#/>). Non-reads keep the shorter
    // stable-window approach (fast commands, no giant streams).
    const isRead = isFileReadCommand(cmd);
    const deadline = Date.now() + (isRead ? 60000 : 30000);
    const stableMs = isRead ? 2500 : 800;
    const lastLine = (s: string) => (s.split("\n").filter(Boolean).pop() ?? "").trimEnd();
    const isPromptLine = (s: string) => /[$#>]\s*$/.test(s);
    let last = terminalText(term, 2000);
    let stableSince = Date.now();
    let markerLine = -1;
    while (Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 250));
      const nowText = terminalText(term, 2000);
      const line = lastLine(nowText);
      markerLine = lastLineStartingWith(term, markerPrefix);
      // Prompt returned after the echo + output → command is done.
      if (isRead && markerLine >= 0 && isPromptLine(line)) break;
      if (nowText === last) {
        if (Date.now() - stableSince > stableMs) break;
      } else {
        last = nowText;
        stableSince = Date.now();
      }
    }
    // From the command banner to the current end of the buffer — full output,
    // then trimmed to a compact digest if it's large (see buildOutputDigest).
    // File reads (cat/sed/awk/head/tail of a file) keep the FULL content so the
    // AI can actually review config/compose files instead of a head/tail digest.
    const raw = terminalTextFrom(
      term,
      markerLine >= 0 ? markerLine : term.buffer.active.length,
      isRead ? 9000 : 3000,
    );
    const mode = isRead ? "read" : "output";
    const { text, truncated, lines, complete } = buildOutputDigest(raw, 8000, mode);
    // DEBUG: show the AI exactly what we're handing back, so truncation (if
    // any) is visible in the terminal instead of mysterious.
    term.write(
      `\r\n\x1b[90m[puppetterm] returned ${utf8Bytes(raw)} bytes / ${lines} lines ` +
        `(mode=${mode}, truncated=${truncated})\x1b[0m\r\n`,
    );
    return {
      host: host || null,
      note: truncated
        ? `typed into the live terminal; output was ${lines} lines and is trimmed to a digest (run grep/tail/head/wc to narrow it)`
        : "typed into the live terminal and waited for the output to settle",
      command: cmd,
      output: text,
      truncated,
      complete,
      total_lines: lines,
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
      const raw = terminalText(term, 1000);
      // This is a SCREEN SNAPSHOT, not a file: the xterm buffer is bounded by
      // scrollback, so it can NEVER be guaranteed "complete". Label it honestly
      // (NOT as a `[file: … COMPLETE …]` read — that marker would make the AI
      // believe it has the whole file when the screen may have scrolled past
      // it, which is exactly how "the output looks truncated" reports started).
      // Keep as much as fits so on-screen file views are still usable.
      const screenCap = 100000;
      const bytes = utf8Bytes(raw);
      const truncated = bytes > screenCap;
      const body = truncated ? raw.slice(0, screenCap) : raw;
      const text = truncated
        ? `[terminal screen: ${bytes} bytes — TRUNCATED at ${screenCap} bytes; the screen is bounded by scrollback, this is NOT a complete file]\n${body}`
        : `[terminal screen: ${bytes} bytes — what is on the user's screen right now, bounded by scrollback (NOT a file, NOT full command output)]\n${body}`;
      term.write("\r\n\x1b[36m[puppetterm] AI read the active terminal…\x1b[0m\r\n");
      return {
        host: host || null,
        note: "live terminal screen (not shell history) — a screen snapshot bounded by scrollback. To get a FILE's full contents or a command's full output, run the command via the `terminal` tool (or the agent tools in agent mode) instead of reading the screen.",
        terminal: text,
        truncated,
        complete: false, // a screen snapshot is NEVER a guaranteed-complete file
        total_lines: raw.split("\n").length,
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
    // Silent mode: in agent mode we show a single concise status line — WHAT the
    // AI ran — not the raw args (which can be huge for config writes) and not
    // the file contents / output bytes. The full result goes to the model via
    // the tool message; the terminal stays clean.
    if (term) {
      term.write(`\r\n\x1b[36m[puppetterm] AI → ${describeAgentAction(name, args)}\x1b[0m\r\n`);
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
    const agentProblem =
      agentErr != null ||
      /permission denied|publickey|connection (?:refused|timed out)|no route to host|could not resolve hostname/i.test(
        res?.error ?? "",
      );
    if (agentProblem) {
      const cmd = structuredToolToShell(name, args);
      // The agent binary is genuinely missing on this host (or auth failed) —
      // correct our cached belief so the badge flips to terminal mode and the
      // AI stops being offered agent tools it can't use.
      if (/not found|No such file|Permission denied|publickey|refused|timed out/i.test(agentErr ?? res?.error ?? "")) {
        agentMap[host] = false;
        agentChecked.delete(host);
      }
      // File reads (config read / log tail / run_command cat) are served by a
      // DIRECT ssh capture: a dedicated ssh exec returns every byte, no pty, no
      // terminal typing. If that ssh route also fails (auth/network), fall
      // through to the agent-unavailable error below — agent mode never types
      // into the live terminal.
      const isRead =
        (name === "config" && String(args.op ?? "") === "read") ||
        name === "log" ||
        (name === "run_command" && isFileReadCommand(String(args.cmd ?? "")));
      if (cmd && isRead) {
        try {
          const cap = await call<any>("ssh_capture", { host, cmd });
          const authProblem =
            cap == null ||
            cap.exit == null ||
            /permission denied|publickey|connection (?:refused|timed out)|no route to host|could not resolve hostname/i.test(
              String(cap?.stderr ?? ""),
            );
          if (!authProblem) {
            const rawOut = String(cap.stdout ?? "");
            const { text, truncated, lines, complete } = buildOutputDigest(rawOut, 8000, "read");
            term?.write(
              `\r\n\x1b[90m[puppetterm] read ${lines} lines / ${utf8Bytes(rawOut)} bytes over ssh\x1b[0m\r\n`,
            );
            return {
              host,
              exit: cap.exit,
              outputs: text,
              output_truncated: truncated,
              output_lines: lines,
              complete,
              via: "ssh-capture",
              fallback: true,
              from: name,
            };
          }
          // ssh auth failed → fall through to the agent-unavailable error below.
        } catch (e) {
          // ssh unavailable → fall through to the agent-unavailable error below.
          console.warn("ssh_capture failed:", e);
        }
      }
      if (cmd) {
        const why = (agentErr ?? res?.error ?? "ssh failure").slice(0, 140);
        // AGENT MODE NEVER TYPES INTO THE USER'S TERMINAL. If the remote agent
        // binary is missing (or the agent route failed), don't silently fall
        // back to typing the command into the live pty — that is exactly the
        // confusing "agent mode is writing to my terminal" behaviour. Return a
        // clear, model-visible error so the AI tells the user to install the
        // agent (or the user relies on terminal mode, where the `terminal` tool
        // legitimately types).
        term?.write(
          `\r\n\x1b[33m[puppetterm] agent unavailable on ${host} (${why}) — NOT typed into your terminal; install the agent to use agent mode\x1b[0m\r\n`,
        );
        return {
          host,
          command: cmd,
          via: "agent-unavailable",
          fallback: false,
          error:
            `The puppetterm-agent is not available on ${host}: ${why}. ` +
            `In agent mode I do NOT type commands into your terminal. ` +
            `To use agent mode here, install the agent (click "Install agent" in the AI panel). ` +
            `If you just need a quick check without the agent, the app can switch to terminal mode — ` +
            `but I will not type into the terminal automatically in agent mode.`,
        };
      }
    }
    if (agentErr) throw new Error(agentErr);
    // Agent mode: the AI already gets the full result via the tool message, so
    // DON'T stream the raw agent stdout into the terminal — that would pollute
    // the terminal buffer, and if the AI later calls `read_terminal`/`terminal`
    // it would pull a big dump back into the context window. A concise status
    // line keeps the user informed without the token cost.
    const resultEvent = [...(res?.events ?? [])].reverse().find((e: any) => e?.type === "result");
    const rawOutputs = (res?.events ?? [])
      .filter((e: any) => e?.type === "output")
      .map((e: any) => e.data ?? "")
      .join("");
    if (term) {
      const exit = resultEvent?.exit ?? res?.exit ?? null;
      // Silent mode: just confirm it finished — no byte counts, no contents.
      term.write(`\r\n\x1b[90m[puppetterm] done (exit ${exit ?? "?"})\x1b[0m\r\n`);
    }
    if (res?.error && term) {
      term.write(`\r\n\x1b[31m[puppetterm] action error: ${res.error}\x1b[0m\r\n`);
    }
    // Same digest as the live-terminal path: large agent output is trimmed to
    // head + tail + error/warning lines so it doesn't burn tokens. EXCEPT for
    // file/log reads (config read, log tail, or run_command running cat/sed/
    // head/tail on a file) — those keep the full content so the AI can actually
    // review configs and logs instead of a digest.
    const isRead =
      (name === "config" && String(args.op ?? "") === "read") ||
      name === "log" ||
      (name === "run_command" && isFileReadCommand(String(args.cmd ?? "")));
    const { text, truncated, lines, complete } = buildOutputDigest(rawOutputs, 8000, isRead ? "read" : "output");
    return {
      host,
      exit: resultEvent?.exit ?? res?.exit ?? null,
      outputs: text,
      output_truncated: truncated,
      output_lines: lines,
      complete,
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
              if (t.host) agentChecked.delete(t.host);
              t.host = ""; // the session is gone — drop the host so the dot goes grey
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
        aiAuthMethod = v.auth_method ?? "key";
        if (v.oauth) {
          oauthAuthUrl = v.oauth.auth_url ?? "";
          oauthTokenUrl = v.oauth.token_url ?? "";
          oauthClientId = v.oauth.client_id ?? "";
          oauthScope = v.oauth.scope ?? "";
          oauthRedirectUri = v.oauth.redirect_uri ?? "";
          oauthFlow = v.oauth.flow ?? "standard";
          oauthHasSecret = !!v.oauth.has_client_secret;
        }
        aiReady = true;
        if (aiProvider === "openai") customBaseUrl = v.base_url || "";
        setHistory(chatHostOf(activeHost), [{ role: "system", content: SYSTEM_PROMPT }]);
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

    </nav>

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

    <span class="tab-actions">
      <button class="refresh" onclick={loadHosts} title="Refresh hosts">↻</button>
      <button class="settings-btn" onclick={() => (showSettings = true)} title="Settings">⚙</button>
    </span>
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
        <button
          class="ai-settings-link"
          onclick={dumpChat}
          disabled={chatLog.length === 0}
          title="Save this conversation to a local Markdown file"
        >
          ⭳ dump
        </button>
        <button
          class="ai-settings-link"
          onclick={dumpChatJson}
          disabled={chatLog.length === 0}
          title="Save the full raw session (incl. tool outputs) to a local JSON file"
        >
          ⭳ json
        </button>
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
          {#if hostHasAgent(activeHost)}
            <span class="mode-badge agent">agent</span>
          {:else if agentMap[activeHost] === false}
            <span class="mode-badge terminal">terminal</span>
          {/if}
          {#if activeTabCwd}
            <span class="ai-cwd" title="Current working directory on the host">{activeTabCwd}</span>
          {/if}
          {#if hostHasAgent(activeHost) && liveMetrics}
            <span class="ai-metrics" title="Live host resources (via agent)">
              CPU {liveMetrics.cpu.toFixed(0)}% · MEM {liveMetrics.mem.toFixed(0)}%
              {#if liveMetrics.load != null}· load {liveMetrics.load.toFixed(2)}{/if}
            </span>
          {/if}
          {#if chatBusy && chatTarget && chatTarget.host !== activeHost}
            <span class="warn">(pinned — you switched tabs)</span>
          {/if}
        {:else}
          local — ssh to a remote first
        {/if}
        {#if activeHost && !installBusy && !hostHasAgent(activeHost)}
          <button
            class="install-agent"
            onclick={promptInstall}
            title="Install puppetterm-agent on {activeHost} (no sudo, reuses your SSH connection)"
          >
            Install agent
          </button>
        {/if}
        {#if activeHost && !installBusy && hostHasAgent(activeHost)}
          <button
            class="install-agent update"
            onclick={promptUpdateAgent}
            title="Update puppetterm-agent on {activeHost} (reinstall from the current build)"
          >
            ↻ Update agent
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
              <div class="activity-row" onclick={() => toggleActivity(a.id)} title="Click to expand details">
                <span class="a-time">{a.ts.slice(11, 19)}</span>
                <span class="a-src {a.source}">{a.source}</span>
                <span class="a-host">{a.host}</span>
                <span class="a-action">{describeAction(a)}</span>
                <span class="a-exit {a.exit === 0 ? 'ok' : 'bad'}">
                  {a.exit == null ? '-' : 'exit ' + a.exit}
                </span>
              </div>
              {#if expandedActivityId === a.id}
                <div class="activity-detail">
                  <div><b>source:</b> {a.source} · <b>approval:</b> {a.approval}</div>
                  {#if a.params}
                    <div class="ad-label">params</div>
                    <pre class="ad-pre">{prettyParams(a.params)}</pre>
                  {/if}
                  {#if a.result}
                    <div class="ad-label">result (summary)</div>
                    <pre class="ad-pre">{prettyParams(a.result)}</pre>
                  {/if}
                  {#if activityDetails[a.id] === "loading"}
                    <div class="ad-label">full output</div>
                    <pre class="ad-pre">loading…</pre>
                  {:else if activityDetails[a.id]}
                    <div class="ad-label">full output</div>
                    <pre class="ad-pre">{prettyParams(activityDetails[a.id])}</pre>
                  {:else if activityDetails[a.id] === null}
                    <div class="ad-label">full output</div>
                    <pre class="ad-pre">no detailed output recorded for this entry</pre>
                  {/if}
                </div>
              {/if}
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
        <textarea
          rows="3"
          placeholder={activeHost
            ? `Ask the AI to act on ${activeHost}…`
            : "Ask the AI to act on a remote — ssh to it first…"}
          bind:value={chatText}
          oninput={(e) => autoGrowInput(e.currentTarget)}
          onkeydown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              sendChat();
            }
          }}
        ></textarea>
        <button onclick={sendChat} disabled={!chatText.trim() || chatBusy}>
          {chatBusy ? "…" : "Send"}
        </button>
        {#if chatBusy}
          <button class="abort-btn" onclick={abortAi} title="Stop the AI and kill the running command">
            Abort
          </button>
        {/if}
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
        class="modal modal-large"
        role="dialog"
        aria-label="Settings"
        tabindex="-1"
        onclick={(e) => e.stopPropagation()}
        onkeydown={(e) => e.stopPropagation()}
      >
        <div class="modal-header">
          <div class="modal-title">Settings</div>
          <button class="modal-close" onclick={() => (showSettings = false)} aria-label="Close">×</button>
        </div>
        <div class="modal-body">
          <nav class="settings-nav">
            <button class:active={activeSettingsTab === "api"} onclick={() => { activeSettingsTab = "api"; setAiAuthMethod("key"); }}>API Providers</button>
            <button class:active={activeSettingsTab === "oauth"} onclick={() => { activeSettingsTab = "oauth"; setAiAuthMethod("oauth"); }}>Web Login</button>
            <button class:active={activeSettingsTab === "models"} onclick={() => (activeSettingsTab = "models")}>Models</button>
            <button class:active={activeSettingsTab === "general"} onclick={() => (activeSettingsTab = "general")}>General</button>
          </nav>
          <div class="settings-content">
            {#if activeSettingsTab === "api"}
              <div class="modal-section">API Providers — per-provider presets</div>
              <div class="provider-grid">
                {#each Object.entries(AI_PROVIDERS) as [key, p] (key)}
                  <div class="provider-card" class:active={aiProvider === key}>
                    <div class="pc-title">{p.label}</div>
                    <div class="pc-meta">{p.baseUrl || "custom endpoint"} · {(p.models[0] ?? "custom model")}</div>
                    <button class:active={aiProvider === key} onclick={() => applyAiProvider(key)}>{aiProvider === key ? "Selected" : "Select"}</button>
                  </div>
                {/each}
              </div>
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
                <input
                  bind:value={aiModel}
                  placeholder={modelsList.length ? "select a model" : "model-name — save to auto-fetch"}
                  list="ai-model-list"
                />
                <datalist id="ai-model-list">
                  {#each allModels as m (m)}
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
              <div class="modal-inline">
                <button
                  type="button"
                  onclick={testAiConfig}
                  disabled={aiTestBusy || !aiBaseUrl.trim() || (!aiKey.trim() && !aiHasKey)}
                >
                  {aiTestBusy ? "Testing…" : "Test connection"}
                </button>
                {#if aiTest}
                  <span class="ai-test {aiTest.ok ? 'ok' : 'err'}">
                    {aiTest.ok ? "✓ " : "✗ "}{aiTest.msg}
                  </span>
                {/if}
              </div>
            {:else if activeSettingsTab === "oauth"}
              <div class="modal-section">Web Login (OAuth) — per-provider presets</div>
              <p class="modal-hint">Log in through the provider's browser page — the bearer token is stored encrypted. If you use Chrome, it reuses the Google account already signed in.</p>
              <div class="provider-grid">
                {#each Object.entries(AI_OAUTH_PRESETS) as [key, p] (key)}
                  <div class="provider-card" class:active={oauthFlow === p.flow && oauthAuthUrl === p.authUrl}>
                    <div class="pc-title">{p.label}</div>
                    <div class="pc-meta">{p.baseUrl}</div>
                    <button onclick={() => applyOauthPreset(key)}>Use</button>
                  </div>
                {/each}
                <div class="provider-card">
                  <div class="pc-title">Custom OAuth</div>
                  <div class="pc-meta">any OpenAI-compatible provider with standard PKCE</div>
                  <button onclick={() => { oauthAuthUrl = ""; oauthTokenUrl = ""; oauthClientId = ""; oauthScope = ""; oauthFlow = "standard"; }}>Clear</button>
                </div>
              </div>
              <label class="modal-field">
                Endpoint
                <input bind:value={aiBaseUrl} placeholder="http://host:port/v1" />
              </label>
              <label class="modal-field">
                Model
                <input
                  bind:value={aiModel}
                  placeholder={modelsList.length ? "select a model" : "model-name — save to auto-fetch"}
                  list="ai-model-list-oauth"
                />
                <datalist id="ai-model-list-oauth">
                  {#each allModels as m (m)}
                    <option value={m}></option>
                  {/each}
                </datalist>
              </label>
              <label class="modal-field">
                OAuth authorize URL
                <input bind:value={oauthAuthUrl} placeholder="https://provider.example/oauth/authorize" />
              </label>
              <label class="modal-field">
                OAuth token URL
                <input bind:value={oauthTokenUrl} placeholder="https://provider.example/oauth/token" />
              </label>
              <label class="modal-field">
                Client ID
                <input bind:value={oauthClientId} placeholder="client id from your OAuth app" />
              </label>
              <label class="modal-field">
                Client secret (optional — confidential clients only)
                <input
                  bind:value={oauthClientSecret}
                  type="password"
                  placeholder={oauthHasSecret ? "••• (set — encrypted)" : "leave blank for PKCE public clients"}
                />
              </label>
              <label class="modal-field">
                Scope (optional)
                <input bind:value={oauthScope} placeholder="e.g. openid profile email" />
              </label>
              <label class="modal-field">
                Redirect URI (auto-filled — must match the OAuth app)
                <input bind:value={oauthRedirectUri} placeholder="http://host:8080/oauth/callback" />
              </label>
              <div class="modal-inline">
                <button
                  type="button"
                  onclick={startOauthLogin}
                  disabled={aiOauthBusy || !oauthAuthUrl.trim() || !oauthTokenUrl.trim() || (!oauthClientId.trim() && oauthFlow !== "openrouter") || !aiBaseUrl.trim() || !aiModel.trim()}
                >
                  {aiOauthBusy ? "Opening login…" : aiHasKey && aiAuthMethod === "oauth" ? "Log in again" : "Log in"}
                </button>
                {#if aiHasKey && aiAuthMethod === "oauth"}
                  <span class="ai-test ok">✓ token set (encrypted)</span>
                {/if}
              </div>
              <div class="modal-inline" style="margin-top:4px">
                <button
                  type="button"
                  onclick={testAiConfig}
                  disabled={aiTestBusy || !aiBaseUrl.trim() || (!aiKey.trim() && !aiHasKey)}
                >
                  {aiTestBusy ? "Testing…" : "Test connection"}
                </button>
                {#if aiTest}
                  <span class="ai-test {aiTest.ok ? 'ok' : 'err'}">
                    {aiTest.ok ? "✓ " : "✗ "}{aiTest.msg}
                  </span>
                {/if}
              </div>
            {:else if activeSettingsTab === "models"}
              <div class="modal-section">Models — fetched from the provider</div>
              {#if !aiHasKey}
                <p class="modal-hint">Configure and authenticate a provider first, then refresh.</p>
              {:else}
                <div class="modal-inline">
                  <button type="button" onclick={loadModels} disabled={modelsLoading}>{modelsLoading ? "Loading…" : "Refresh from provider"}</button>
                  {#if modelsError}<span class="ai-test err">{modelsError}</span>{/if}
                </div>
                <div class="modal-inline" style="margin-top:6px">
                  <button type="button" onclick={() => (enabledModels = [...modelsList])} disabled={!modelsList.length}>Enable all</button>
                  <button type="button" onclick={() => (enabledModels = [])} disabled={!enabledModels.length}>Disable all</button>
                  <span class="modal-hint" style="margin-left:6px">{enabledModels.length}/{modelsList.length} enabled</span>
                </div>
                {#if modelsList.length}
                  <div class="model-list">
                    {#each modelsList as m (m)}
                      <label class="model-row">
                        <input type="checkbox" checked={enabledModels.includes(m)} onchange={() => toggleModel(m)} />
                        <span>{m}</span>
                      </label>
                    {/each}
                  </div>
                {:else if !modelsError}
                  <p class="modal-hint">No models fetched yet — click Refresh.</p>
                {/if}
                <p class="modal-hint" style="margin-top:8px">Disabled models are hidden from the chat model switcher. The current model ({aiModel || "none"}) always stays visible.</p>
              {/if}
            {:else}
              <div class="modal-section">General</div>
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
            {/if}
          </div>
        </div>

        <div class="modal-btns">
          <button class="danger" type="button" onclick={deleteAiConfig}>
            Delete provider
          </button>
          <div class="spacer"></div>
          <button onclick={() => (showSettings = false)}>Cancel</button>
          <button class="primary" onclick={saveSettings} disabled={!aiBaseUrl.trim()}>Save</button>
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
    flex-wrap: nowrap;
    align-items: center;
    gap: 4px;
    padding: 0 8px;
    overflow-x: auto;
    overflow-y: hidden;
    flex: 1;
    min-width: 0;
    height: 100%;
    scrollbar-width: thin;
  }
  .tab-actions {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 0 10px 0 4px;
    flex: none;
  }
  .tab {
    display: flex;
    align-items: center;
    gap: 7px;
    flex: 0 0 auto;
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
    align-items: center;
    flex: none;
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
    gap: 6px;
    margin: 0;
    padding: 6px 12px 10px;
    flex: none;
    border-top: 1px solid #21262d;
  }
  .ai-provider-tag {
    font-size: 10px;
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
    padding: 3px 6px;
    font-size: 11px;
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
  .chat-input textarea {
    background: #0d1117;
    border: 1px solid #30363d;
    border-radius: 6px;
    color: #e6edf3;
    padding: 6px 8px;
    font-size: 13px;
    outline: none;
  }
  .chat-input textarea:focus {
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
    max-height: 120px; /* long commands scroll internally so the buttons stay visible */
    overflow-y: auto;
  }
  .approval-explain {
    font-size: 12px;
    color: #e6edf3;
    background: #0d1117;
    border-radius: 6px;
    padding: 6px 8px;
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 100px; /* long AI explanations scroll internally too */
    overflow-y: auto;
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
    cursor: pointer;
  }
  .activity-row:hover {
    background: #161b22;
  }
  .activity-row:last-child {
    border-bottom: none;
  }
  .a-src {
    font-size: 10px;
    padding: 0 4px;
    border-radius: 4px;
    text-transform: uppercase;
  }
  .a-src.ai {
    color: #bc8cff;
    background: #21262d;
  }
  .a-src.user {
    color: #58a6ff;
    background: #21262d;
  }
  .activity-detail {
    padding: 6px 8px;
    background: #010409;
    border-bottom: 1px solid #161b22;
    font-size: 11px;
    color: #c9d1d9;
  }
  .ad-label {
    color: #8b949e;
    margin: 4px 0 2px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    font-size: 10px;
  }
  .ad-pre {
    margin: 0;
    padding: 6px;
    background: #0d1117;
    border: 1px solid #21262d;
    border-radius: 4px;
    white-space: pre-wrap;
    word-break: break-word;
    font-family: monospace;
    max-height: 200px;
    overflow-y: auto;
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
  .modal.modal-large {
    width: 900px;
    max-width: 92vw;
    max-height: 88vh;
    display: flex;
    flex-direction: column;
    padding: 0;
    overflow: hidden;
  }
  .modal-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 16px 20px 12px;
    border-bottom: 1px solid #21262d;
    flex: none;
  }
  .modal-header .modal-title {
    margin-bottom: 0;
  }
  .modal-close {
    background: transparent;
    border: 1px solid #30363d;
    color: #8b949e;
    border-radius: 6px;
    width: 28px;
    height: 28px;
    cursor: pointer;
    font-size: 18px;
    line-height: 1;
    display: grid;
    place-items: center;
  }
  .modal-close:hover {
    background: #21262d;
    color: #e6edf3;
  }
  .modal-body {
    display: flex;
    flex: 1;
    min-height: 0;
    overflow: hidden;
  }
  .settings-nav {
    width: 160px;
    flex: none;
    background: #010409;
    border-right: 1px solid #21262d;
    display: flex;
    flex-direction: column;
    padding: 12px 8px;
    gap: 6px;
  }
  .settings-nav button {
    background: transparent;
    border: 1px solid transparent;
    color: #8b949e;
    border-radius: 6px;
    padding: 8px 10px;
    text-align: left;
    cursor: pointer;
    font-size: 13px;
    font-weight: 600;
  }
  .settings-nav button.active {
    background: #21262d;
    border-color: #30363d;
    color: #e6edf3;
  }
  .settings-nav button:hover {
    background: #161b22;
  }
  .settings-content {
    flex: 1;
    overflow-y: auto;
    padding: 16px 20px;
  }
  .modal-hint {
    font-size: 12.5px;
    color: #8b949e;
    margin: -4px 0 12px;
    line-height: 1.4;
  }
  .provider-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(200px, 1fr));
    gap: 10px;
    margin-bottom: 16px;
  }
  .provider-card {
    background: #010409;
    border: 1px solid #30363d;
    border-radius: 8px;
    padding: 12px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .provider-card.active {
    border-color: #1f6feb;
    background: #0d1a33;
  }
  .provider-card .pc-title {
    font-size: 13px;
    font-weight: 700;
    color: #e6edf3;
  }
  .provider-card .pc-meta {
    font-size: 11.5px;
    color: #8b949e;
    line-height: 1.3;
    min-height: 28px;
    word-break: break-word;
  }
  .provider-card button {
    align-self: flex-start;
    border: 1px solid #30363d;
    background: #21262d;
    color: #e6edf3;
    border-radius: 6px;
    padding: 6px 12px;
    cursor: pointer;
    font-size: 12px;
    font-weight: 600;
  }
  .provider-card button.active {
    background: #1f6feb;
    border-color: #1f6feb;
    color: #fff;
  }
  .settings-content .modal-section:first-child {
    margin-top: 0;
    border-top: none;
    padding-top: 0;
  }
  .model-list {
    max-height: 260px;
    overflow-y: auto;
    border: 1px solid #21262d;
    border-radius: 8px;
    padding: 6px;
    background: #010409;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .model-row {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 13px;
    color: #e6edf3;
    padding: 4px 6px;
    border-radius: 6px;
  }
  .model-row:hover {
    background: #161b22;
  }
  .model-row input {
    accent-color: #1f6feb;
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
  .modal-btns .spacer {
    flex: 1;
  }
  .modal-btns button.danger {
    background: #21262d;
    border-color: #6e2a2a;
    color: #f0883e;
  }
  .modal-btns button.danger:hover {
    background: #3d1518;
  }
  .modal-inline {
    display: flex;
    align-items: center;
    gap: 10px;
    margin: 8px 0 4px;
  }
  .modal-inline button {
    border: 1px solid #30363d;
    background: #21262d;
    color: #e6edf3;
    border-radius: 6px;
    padding: 7px 14px;
    cursor: pointer;
    font-weight: 600;
  }
  .modal-inline button:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .ai-test {
    font-size: 12px;
    word-break: break-word;
  }
  .ai-test.ok {
    color: #3fb950;
  }
  .ai-test.err {
    color: #f85149;
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
  .mode-badge {
    font-size: 9px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    padding: 1px 6px;
    border-radius: 999px;
    margin-left: 2px;
  }
  .mode-badge.agent {
    color: #3fb950;
    background: #12261b;
    border: 1px solid #238636;
  }
  .mode-badge.terminal {
    color: #8b949e;
    background: #161b22;
    border: 1px solid #30363d;
  }
  .ai-cwd {
    font-family: monospace;
    font-size: 11px;
    color: #58a6ff;
    background: #0d1117;
    border: 1px solid #30363d;
    border-radius: 6px;
    padding: 1px 6px;
    max-width: 180px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .ai-metrics {
    font-family: monospace;
    font-size: 11px;
    color: #3fb950;
    background: #0d1117;
    border: 1px solid #30363d;
    border-radius: 6px;
    padding: 1px 6px;
    white-space: nowrap;
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
  .install-agent.update {
    border-color: #d29922;
    color: #d29922;
  }
  .install-agent.update:hover {
    background: #d29922;
    color: #000;
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
    align-items: flex-end;
    gap: 6px;
    padding: 10px 12px;
    border-top: 1px solid #21262d;
  }
  .chat-input textarea {
    flex: 1;
    resize: none; /* height is driven by content (auto-grow), capped at max-height */
    overflow-y: auto;
    min-height: 64px; /* ~3 lines so you can see what you're typing */
    max-height: 200px; /* "max standard" — beyond this it scrolls */
    line-height: 1.45;
    font-family: inherit;
    box-sizing: border-box;
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
