/**
 * Chat store — manages multi-conversation AI chat state with server-side
 * SQLite persistence. Each SSH host (and the local session) can have
 * multiple named conversations.
 *
 * localStorage remains as a fast cache; the server DB is the source of truth.
 */
import { call } from "./backend";

export interface ChatMessage {
  role: string;
  content?: string;
  tool_call_id?: string;
  tool_calls?: any;
}

export interface ConversationMeta {
  id: string;
  host: string;
  title: string;
  created_at: string;
  updated_at: string;
  message_count: number;
}

interface ChatEntry {
  chatLog: { role: string; text: string }[];
  history: ChatMessage[];
}

// ---- helpers ---------------------------------------------------------------

function chatHostOf(h: string | null | undefined): string {
  return h && h.length ? h : "__local__";
}

function localStorageKey(host: string): string {
  return `pp.chat.${host}.session`;
}

function loadFromLocalStorage(host: string): { entry: ChatEntry; id: string | null; savedAt: number } | null {
  if (typeof localStorage === "undefined") return null;
  const raw = localStorage.getItem(localStorageKey(host));
  if (!raw) return null;
  try {
    const v = JSON.parse(raw);
    if (v && Array.isArray(v.chatLog)) {
      return {
        entry: {
          chatLog: v.chatLog,
          history: Array.isArray(v.history) ? v.history : [],
        },
        id: typeof v.convId === "string" ? v.convId : null,
        savedAt: typeof v.savedAt === "number" ? v.savedAt : 0,
      };
    }
  } catch { /* corrupt */ }
  return null;
}

// LocalStorage cache is bounded so a long conversation never blows the ~5MB
// quota (which would silently drop the cache and look like the chat "reset").
// The server DB is the source of truth; the cache only needs to be enough to
// restore the visible conversation + recent LLM context on reload.
const CACHE_MAX_LOG = 300;
const CACHE_MAX_HISTORY = 120;

function saveToLocalStorage(host: string, entry: ChatEntry, convId: string | null) {
  if (typeof localStorage === "undefined") return;
  const chatLog = entry.chatLog.slice(-CACHE_MAX_LOG);
  // Keep the leading system prompt (first message) + recent history so the AI
  // can restore its task context even on the tail of a very long thread.
  const history =
    entry.history.length > CACHE_MAX_HISTORY
      ? [entry.history[0], ...entry.history.slice(-(CACHE_MAX_HISTORY - 1))]
      : entry.history;
  try {
    localStorage.setItem(
      localStorageKey(host),
      JSON.stringify({ chatLog, history, host, convId, savedAt: Date.now() }),
    );
  } catch { /* quota */ }
}

// ---- store -----------------------------------------------------------------

export class ChatStore {
  /** Per-host chat entries (chatLog + history). Keyed by resolved host string. */
  entries: Record<string, ChatEntry> = {};

  /** All conversations for the current host, newest first. */
  conversations: ConversationMeta[] = $state([]);

  /** The currently loaded conversation id (null = unsaved new chat). */
  activeConversationId: string | null = $state(null);

  /** Currently loaded visible chat log. */
  chatLog: { role: string; text: string }[] = $state([]);

  /** The currently loaded LLM history (system + user + assistant + tool messages). */
  history: ChatMessage[] = $state([]);

  /** Guards against an older host's async restore landing after a newer one. */
  private restoreSeq = 0;

  /** The host the currently displayed conversation belongs to (null = none). */
  stateHost: string | null = $state(null);

  // ---- conversation list ---------------------------------------------------

  /** Fetch the conversation list for a host from the server. */
  async listConversations(host: string): Promise<ConversationMeta[]> {
    const h = chatHostOf(host);
    try {
      const resp = await call<{ conversations: ConversationMeta[] }>("chat_list_conversations", { host: h });
      this.conversations = resp.conversations ?? [];
      return this.conversations;
    } catch {
      this.conversations = [];
      return [];
    }
  }

  /** Create a new conversation on the server. Returns the id. */
  async createConversation(host: string, title?: string): Promise<string> {
    const h = chatHostOf(host);
    const resp = await call<{ id: string }>("chat_new", { host: h, title: title ?? "" });
    return resp.id;
  }

  /** Load a conversation from the server into the active state. */
  async loadConversation(id: string): Promise<void> {
    const resp = await call<{ meta: ConversationMeta; messages: ChatMessage[] }>("chat_load", { id });
    this.activeConversationId = id;
    this.history = resp.messages ?? [];
    // Rebuild chatLog from history (user + assistant messages only)
    this.chatLog = this.history
      .filter((m) => m.role === "user" || m.role === "assistant")
      .map((m) => ({
        role: m.role === "assistant" ? "ai" : m.role,
        text: m.content ?? "",
      }));
  }

  /** Restore the conversation to show for a host.
   *
   *  The server DB is the source of truth, but a long conversation must also be
   *  resumable from a fresh browser/device where the localStorage cache is
   *  empty. Prefer the newest server conversation unless the local cache is
   *  strictly newer (e.g. the last server write failed) — otherwise a stale
   *  cache would overwrite newer server history on the next save. */
  async restoreForHost(host: string, isBusy?: () => boolean): Promise<void> {
    const seq = ++this.restoreSeq;
    const h = chatHostOf(host);
    const cached = loadFromLocalStorage(h);
    let newest: ConversationMeta | null = null;
    try {
      newest = (await this.listConversations(h))[0] ?? null;
    } catch {
      newest = null; // server unreachable — fall back to the cache
    }
    if (seq !== this.restoreSeq || isBusy?.()) return; // superseded or a turn started
    const serverTs = newest ? Date.parse(newest.updated_at) : NaN;
    const cacheTs = cached?.savedAt ?? 0;
    const serverIsFresh =
      newest != null && (cached == null || Number.isNaN(cacheTs) || serverTs >= cacheTs);
    if (newest && serverIsFresh) {
      await this.loadConversation(newest.id);
      if (seq !== this.restoreSeq || isBusy?.()) return;
      this.stateHost = h;
      saveToLocalStorage(
        h,
        { chatLog: this.chatLog, history: this.history },
        this.activeConversationId,
      );
      return;
    }
    if (cached) {
      if (isBusy?.()) return;
      this.chatLog = cached.entry.chatLog;
      this.history = cached.entry.history;
      this.activeConversationId = cached.id;
      this.stateHost = h;
      // Cache is newer than the server copy (or the server had none): push it up
      // so the conversation is not stranded in this browser.
      void this.persistConversation(h);
      return;
    }
    if (isBusy?.()) return;
    this.newChat();
    this.stateHost = h;
  }

  /** Delete a conversation from the server. */
  async deleteConversation(id: string): Promise<void> {
    await call("chat_delete", { id });
    if (this.activeConversationId === id) {
      this.activeConversationId = null;
      this.chatLog = [];
      this.history = [];
    }
  }

  /** Rename a conversation on the server. */
  async renameConversation(id: string, title: string): Promise<void> {
    await call("chat_rename", { id, title });
  }

  // ---- active conversation mutations ---------------------------------------

  /** Append a visible chat message (what the user sees). */
  pushChat(role: string, text: string) {
    this.chatLog = [...this.chatLog, { role, text }];
  }

  /** Set the full LLM history for the active conversation. */
  setHistory(next: ChatMessage[]) {
    this.history = next;
  }

  /** Get the current LLM history. */
  getHistory(): ChatMessage[] {
    return this.history;
  }

  /** Save current state to localStorage synchronously (for beforeunload).
   *  Returns true if there was data to save. */
  persistToCache(host: string): boolean {
    if (this.chatLog.length === 0 && this.history.length <= 1) return false;
    const h = chatHostOf(host);
    saveToLocalStorage(h, { chatLog: this.chatLog, history: this.history }, this.activeConversationId);
    return true;
  }

  /** Persist the current conversation to the server (replace mode).
   *  Call this after mutations to sync with the backend. */
  async persistConversation(host: string): Promise<void> {
    const h = chatHostOf(host);
    const entry = { chatLog: this.chatLog, history: this.history };
    let convId = this.activeConversationId;
    // Also cache in localStorage
    saveToLocalStorage(h, entry, convId);
    // A conversation with no user/assistant content is just the seeded system
    // prompt. Don't create server rows for it: switching hosts (or clicking
    // "new chat" and never sending) would otherwise accumulate empty
    // "New chat" entries. The localStorage cache above still preserves it.
    if (!entry.history.some((m) => m.role === "user" || m.role === "assistant")) {
      return;
    }

    if (!convId) {
      // Create a new conversation on the server
      try {
        convId = await this.createConversation(h);
        // Only claim the new id if nothing changed this conversation meanwhile
        // (e.g. we switched hosts and loaded another conversation's cache).
        if (!this.activeConversationId) this.activeConversationId = convId;
      } catch {
        return; // server unavailable, localStorage cache is enough
      }
    }

    // Sync the full history to the server via replace (handles compaction).
    // Use the locally-captured id/history so an overlapping host switch can't
    // make us write the old conversation into the new one's slot.
    try {
      await call("chat_replace_all", {
        conversation_id: convId,
        messages: entry.history,
      });
      // Only re-link the id if the active conversation hasn't changed meanwhile.
      if (!this.activeConversationId) this.activeConversationId = convId;
    } catch { /* server unavailable */ }
  }

  /** Start a fresh conversation (clears active state). */
  newChat() {
    this.activeConversationId = null;
    this.chatLog = [];
    this.history = [];
  }

  /** Restore from localStorage cache; also re-links the server conversation id. */
  loadFromCache(host: string): boolean {
    const h = chatHostOf(host);
    const cached = loadFromLocalStorage(h);
    if (cached) {
      this.chatLog = cached.entry.chatLog;
      this.history = cached.entry.history;
      this.activeConversationId = cached.id;
      return true;
    }
    return false;
  }
}
