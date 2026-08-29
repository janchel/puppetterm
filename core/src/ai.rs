//! AI client — multi-provider chat completions with tool/function calling.
//!
//! Providers:
//!   openai    — custom OpenAI-compatible endpoint (any provider/model)
//!   deepseek  — DeepSeek's OpenAI-compatible endpoint
//!   anthropic — Claude via the Anthropic Messages API
//!
//! Config (provider, base_url, model, encrypted api_key) lives in
//! `~/.config/puppetterm/ai.json` (outside the repo) or in the PUPPETTERM_AI_*
//! environment variables. The API key is **encrypted at rest** (ChaCha20-Poly1305
//! keyed from the machine id) and never crosses into the frontend.

use std::path::PathBuf;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Nonce,
};
use serde::{Deserialize, Serialize};

pub const PROVIDER_OPENAI: &str = "openai"; // custom OpenAI-compatible
pub const PROVIDER_DEEPSEEK: &str = "deepseek";
pub const PROVIDER_ANTHROPIC: &str = "anthropic";

fn default_provider() -> String {
    PROVIDER_OPENAI.to_string()
}

/// AI provider config, stored on disk (never committed). `api_key` is the
/// decrypted key in memory only; `api_key_enc` is what lives on disk.
#[derive(Serialize, Deserialize, Clone)]
pub struct AiConfig {
    pub base_url: String,
    pub model: String,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(skip_serializing, default)]
    pub api_key: String,
    #[serde(default)]
    pub api_key_enc: Option<String>,
}

/// Non-secret view of the config for the settings UI.
#[derive(Serialize)]
pub struct AiConfigView {
    pub base_url: String,
    pub model: String,
    pub provider: String,
    pub has_api_key: bool,
}

// ---- encrypted key storage -------------------------------------------------

/// 32-byte key derived from the machine identity (protects the key file at
/// rest; a fully compromised machine is out of scope).
fn machine_key() -> Result<[u8; 32], String> {
    let id = ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()))
        .or_else(|| std::env::var("HOSTNAME").ok())
        .ok_or_else(|| "cannot derive machine key (no machine-id)".to_string())?;
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"puppetterm-ai-key-v1");
    h.update(id.as_bytes());
    Ok(h.finalize().into())
}

fn encrypt_key(plain: &str) -> Result<String, String> {
    let key = machine_key()?;
    let cipher = ChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(&key));
    let mut nonce_bytes = [0u8; 12];
    getrandom::getrandom(&mut nonce_bytes).map_err(|e| format!("rng: {e}"))?;
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plain.as_bytes())
        .map_err(|_| "encryption failed".to_string())?;
    let mut out = nonce_bytes.to_vec();
    out.extend_from_slice(&ct);
    Ok(B64.encode(&out))
}

fn decrypt_key(enc: &str) -> Result<String, String> {
    let data = B64.decode(enc).map_err(|e| format!("bad encoded key: {e}"))?;
    if data.len() < 12 {
        return Err("key too short".into());
    }
    let (nonce, ct) = data.split_at(12);
    let key = machine_key()?;
    let cipher = ChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(&key));
    let pt = cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| "cannot decrypt api key".to_string())?;
    String::from_utf8(pt).map_err(|_| "key is not utf8".into())
}

// ---- config load/save ------------------------------------------------------

pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("PUPPETTERM_AI_CONFIG") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config").join("puppetterm").join("ai.json")
}

pub fn load_config() -> Result<AiConfig, String> {
    let env_key = std::env::var("PUPPETTERM_AI_API_KEY").ok();
    let env_base = std::env::var("PUPPETTERM_AI_BASE_URL").ok();
    let env_model = std::env::var("PUPPETTERM_AI_MODEL").ok();
    let env_provider = std::env::var("PUPPETTERM_AI_PROVIDER").ok();
    if let (Some(base_url), Some(api_key), Some(model)) = (&env_base, &env_key, &env_model) {
        return Ok(AiConfig {
            base_url: base_url.clone(),
            api_key: api_key.clone(),
            model: model.clone(),
            provider: env_provider.unwrap_or_else(default_provider),
            api_key_enc: None,
        });
    }
    let data = std::fs::read_to_string(config_path())
        .map_err(|e| format!("AI config not found: {e} (set PUPPETTERM_AI_* or create {})", config_path().display()))?;
    let mut cfg: AiConfig = serde_json::from_str(&data).map_err(|e| format!("invalid AI config: {e}"))?;
    // Decrypt the stored key; legacy plaintext `api_key` field also loads.
    if let Some(enc) = &cfg.api_key_enc {
        cfg.api_key = decrypt_key(enc).unwrap_or_default();
    }
    Ok(cfg)
}

pub fn save_config(cfg: &AiConfig) -> Result<(), String> {
    let mut out = cfg.clone();
    if !out.api_key.is_empty() {
        out.api_key_enc = Some(encrypt_key(&out.api_key)?);
    }
    out.api_key = String::new(); // never write the plaintext key to disk
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let data = serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?;
    std::fs::write(&path, data).map_err(|e| e.to_string())
}

// ---- OpenAI chat completion types -----------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolCallFunction,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatMessage {
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionDef,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatResponse {
    pub id: Option<String>,
    pub choices: Vec<ChatChoice>,
    #[serde(default)]
    pub usage: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatChoice {
    pub index: Option<i64>,
    pub finish_reason: Option<String>,
    pub message: Option<ChatMessage>,
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolDef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

/// Call the configured provider's chat completions endpoint (OpenAI-compatible
/// for openai/deepseek, Anthropic Messages for anthropic/claude).
pub async fn chat_completion(
    cfg: &AiConfig,
    messages: Vec<ChatMessage>,
    tools: Option<Vec<ToolDef>>,
    max_tokens: Option<u32>,
) -> Result<ChatResponse, String> {
    if cfg.provider == PROVIDER_ANTHROPIC {
        anthropic_completion(cfg, messages, tools, max_tokens).await
    } else {
        openai_completion(cfg, messages, tools, max_tokens).await
    }
}

/// Update the stored AI config (shared by the desktop and web frontends).
/// Blank fields keep their current values; a blank key keeps the stored one.
pub fn apply_ai_config(
    base_url: String,
    model: String,
    provider: Option<String>,
    api_key: Option<String>,
) -> Result<(), String> {
    let mut cfg = match load_config() {
        Ok(c) => c,
        Err(_) => AiConfig {
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            provider: PROVIDER_OPENAI.into(),
            api_key_enc: None,
        },
    };
    if !base_url.trim().is_empty() {
        cfg.base_url = base_url.trim().to_string();
    }
    if !model.trim().is_empty() {
        cfg.model = model.trim().to_string();
    }
    if let Some(p) = provider {
        let p = p.trim().to_string();
        if !p.is_empty() {
            cfg.provider = if p == PROVIDER_ANTHROPIC || p == PROVIDER_DEEPSEEK || p == PROVIDER_OPENAI
            {
                p
            } else {
                PROVIDER_OPENAI.into()
            };
        }
    }
    if let Some(k) = api_key {
        let k = k.trim();
        if !k.is_empty() {
            cfg.api_key = k.to_string();
        }
    }
    save_config(&cfg)
}

/// Remove the on-disk AI config (clears the configured provider). The in-memory
/// config is untouched; the next `load_config()` simply finds no file.
pub fn delete_config() -> Result<(), String> {
    let path = config_path();
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| format!("cannot delete AI config: {e}"))?;
    }
    Ok(())
}

/// Validate a provider/endpoint/model/key combo with a real (tiny) completion
/// request **without** persisting it. Returns a short success summary, or the
/// upstream error so the UI can surface why the connection failed.
pub async fn test_config(
    base_url: String,
    model: String,
    provider: Option<String>,
    api_key: Option<String>,
) -> Result<String, String> {
    // Start from any stored config so an already-saved key (or field the user
    // didn't retype) is still used for the probe.
    let stored = load_config().ok();
    let base_url = base_url.trim().to_string();
    let model = model.trim().to_string();
    let base_url = if base_url.is_empty() { stored.as_ref().map(|c| c.base_url.clone()).unwrap_or_default() } else { base_url };
    let model = if model.is_empty() { stored.as_ref().map(|c| c.model.clone()).unwrap_or_default() } else { model };
    if base_url.is_empty() || model.is_empty() {
        return Err("base_url and model are required".into());
    }
    let provider = provider.map(|p| p.trim().to_string()).filter(|p| !p.is_empty())
        .or_else(|| stored.as_ref().map(|c| c.provider.clone()))
        .unwrap_or_else(default_provider);
    let provider = match provider.as_str() {
        PROVIDER_ANTHROPIC | PROVIDER_DEEPSEEK | PROVIDER_OPENAI => provider,
        _ => PROVIDER_OPENAI.to_string(),
    };
    let api_key = api_key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
        .or_else(|| stored.as_ref().map(|c| c.api_key.clone()))
        .unwrap_or_default();
    let cfg = AiConfig {
        base_url,
        api_key,
        model: model.clone(),
        provider: provider.clone(),
        api_key_enc: None,
    };
    let resp = chat_completion(
        &cfg,
        vec![ChatMessage {
            role: Role::User,
            content: Some("Reply with exactly the single word: PONG".into()),
            tool_call_id: None,
            tool_calls: None,
        }],
        None,
        Some(16),
    )
    .await?;
    if resp.choices.is_empty() {
        return Err("provider returned an empty response".into());
    }
    Ok(format!("Connected · {} · {}", provider, model))
}

/// Call an OpenAI-compatible `/chat/completions` endpoint (custom + DeepSeek).
async fn openai_completion(
    cfg: &AiConfig,
    messages: Vec<ChatMessage>,
    tools: Option<Vec<ToolDef>>,
    max_tokens: Option<u32>,
) -> Result<ChatResponse, String> {
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let body = ChatRequest {
        model: cfg.model.clone(),
        messages,
        tools,
        tool_choice: Some(serde_json::json!("auto")),
        max_tokens,
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client
        .post(&url)
        .bearer_auth(&cfg.api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("AI request failed: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        let snippet = text.chars().take(500).collect::<String>();
        return Err(format!("AI API {status}: {snippet}"));
    }
    serde_json::from_str(&text).map_err(|e| {
        let snippet = text.chars().take(300).collect::<String>();
        format!("AI response parse error: {e}: {snippet}")
    })
}

// ---- Anthropic (Claude) Messages API ---------------------------------------

#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String, // "user" | "assistant"
    content: serde_json::Value,
}

#[derive(Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

#[derive(Deserialize)]
struct AnthropicBlock {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
    id: Option<String>,
    name: Option<String>,
    input: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicBlock>,
    stop_reason: Option<String>,
}

/// Convert the app's OpenAI-style history into Anthropic messages + system.
/// Tool results are grouped into a single trailing `user` message with
/// `tool_result` blocks (Anthropic requires this right after tool_use).
fn to_anthropic(messages: &[ChatMessage]) -> (Option<String>, Vec<AnthropicMessage>) {
    let mut system: Vec<String> = Vec::new();
    let mut out: Vec<AnthropicMessage> = Vec::new();
    let mut pending_results: Vec<serde_json::Value> = Vec::new();

    let flush_results = |out: &mut Vec<AnthropicMessage>, pending: &mut Vec<serde_json::Value>| {
        if !pending.is_empty() {
            let blocks = std::mem::take(pending);
            out.push(AnthropicMessage {
                role: "user".into(),
                content: serde_json::json!(blocks),
            });
        }
    };

    for m in messages {
        match m.role {
            Role::System => {
                if let Some(c) = &m.content {
                    system.push(c.clone());
                }
            }
            Role::User => {
                flush_results(&mut out, &mut pending_results);
                if let Some(c) = &m.content {
                    out.push(AnthropicMessage {
                        role: "user".into(),
                        content: serde_json::Value::String(c.clone()),
                    });
                }
            }
            Role::Assistant => {
                flush_results(&mut out, &mut pending_results);
                let mut blocks: Vec<serde_json::Value> = Vec::new();
                if let Some(c) = &m.content {
                    blocks.push(serde_json::json!({"type": "text", "text": c}));
                }
                if let Some(tcs) = &m.tool_calls {
                    for tc in tcs {
                        let input = serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                            .unwrap_or(serde_json::Value::Null);
                        blocks.push(serde_json::json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.function.name,
                            "input": input,
                        }));
                    }
                }
                if !blocks.is_empty() {
                    out.push(AnthropicMessage {
                        role: "assistant".into(),
                        content: serde_json::Value::Array(blocks),
                    });
                }
            }
            Role::Tool => {
                pending_results.push(serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                    "content": m.content.clone().unwrap_or_default(),
                }));
            }
        }
    }
    flush_results(&mut out, &mut pending_results);

    let system = if system.is_empty() {
        None
    } else {
        Some(system.join("\n\n"))
    };
    (system, out)
}

/// Call the Anthropic Messages API and normalize the response to the shared
/// OpenAI-style ChatResponse so the frontend is provider-agnostic.
async fn anthropic_completion(
    cfg: &AiConfig,
    messages: Vec<ChatMessage>,
    tools: Option<Vec<ToolDef>>,
    max_tokens: Option<u32>,
) -> Result<ChatResponse, String> {
    let url = format!("{}/messages", cfg.base_url.trim_end_matches('/'));
    let (system, amessages) = to_anthropic(&messages);
    let atools = tools.map(|ts| {
        ts.into_iter()
            .map(|t| AnthropicTool {
                name: t.function.name,
                description: t.function.description,
                input_schema: t.function.parameters,
            })
            .collect::<Vec<_>>()
    });
    let body = AnthropicRequest {
        model: cfg.model.clone(),
        max_tokens: max_tokens.unwrap_or(4096),
        system,
        messages: amessages,
        tools: atools,
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client
        .post(&url)
        .header("x-api-key", &cfg.api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("AI request failed: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        let snippet = text.chars().take(500).collect::<String>();
        return Err(format!("AI API {status}: {snippet}"));
    }
    let parsed: AnthropicResponse = serde_json::from_str(&text).map_err(|e| {
        let snippet = text.chars().take(300).collect::<String>();
        format!("Anthropic response parse error: {e}: {snippet}")
    })?;

    let mut content = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    for b in parsed.content {
        if b.kind == "text" {
            if let Some(t) = b.text {
                content.push_str(&t);
            }
        } else if b.kind == "tool_use" {
            if let (Some(id), Some(name)) = (b.id, b.name) {
                let args = b
                    .input
                    .map(|i| serde_json::to_string(&i).unwrap_or_default())
                    .unwrap_or_default();
                tool_calls.push(ToolCall {
                    id,
                    kind: "function".into(),
                    function: ToolCallFunction { name, arguments: args },
                });
            }
        }
    }
    let finish_reason = match parsed.stop_reason.as_deref() {
        Some("tool_use") => Some("tool_calls".into()),
        _ => Some("stop".into()),
    };
    Ok(ChatResponse {
        id: None,
        choices: vec![ChatChoice {
            index: Some(0),
            finish_reason,
            message: Some(ChatMessage {
                role: Role::Assistant,
                content: if content.is_empty() { None } else { Some(content) },
                tool_call_id: None,
                tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
            }),
        }],
        usage: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_roundtrip() {
        let plain = "sk-test-12345-abcdef";
        let enc = encrypt_key(plain).expect("encrypt");
        assert_ne!(enc, plain, "encrypted value must differ from plaintext");
        assert_eq!(decrypt_key(&enc).expect("decrypt"), plain);
    }

    #[test]
    fn config_encrypts_key_at_rest() {
        let path = std::env::temp_dir().join(format!("puppetterm-ai-test-{}.json", std::process::id()));
        std::env::set_var("PUPPETTERM_AI_CONFIG", path.to_string_lossy().into_owned());
        let cfg = AiConfig {
            base_url: "http://example/v1".into(),
            model: "m".into(),
            provider: PROVIDER_OPENAI.into(),
            api_key: "sk-super-secret-xyz".into(),
            api_key_enc: None,
        };
        save_config(&cfg).expect("save");
        let raw = std::fs::read_to_string(&path).expect("read file");
        assert!(!raw.contains("sk-super-secret-xyz"), "plaintext key leaked into file: {raw}");
        assert!(raw.contains("api_key_enc"), "encrypted key missing from file: {raw}");
        let loaded = load_config().expect("load");
        assert_eq!(loaded.api_key, "sk-super-secret-xyz", "decrypted key mismatch");
        let _ = std::fs::remove_file(&path);
        std::env::remove_var("PUPPETTERM_AI_CONFIG");
    }

    // Live test against the configured endpoint. Skipped unless
    // PUPPETTERM_TEST_AI=1.
    #[tokio::test]
    async fn chat_completion_and_tool_call() {
        if std::env::var("PUPPETTERM_TEST_AI").unwrap_or_default() != "1" {
            eprintln!("skipping; set PUPPETTERM_TEST_AI=1 to hit the live endpoint");
            return;
        }
        let cfg = load_config().expect("ai config");

        // 1. Plain completion.
        let plain = chat_completion(
            &cfg,
            vec![ChatMessage {
                role: Role::User,
                content: Some("Reply with exactly: PONG".into()),
                tool_call_id: None,
                tool_calls: None,
            }],
            None,
            Some(64),
        )
        .await
        .expect("plain completion");
        assert!(!plain.choices.is_empty());
        assert!(plain.choices[0].message.as_ref().is_some());

        // 2. Tool calling.
        let tools = vec![ToolDef {
            kind: "function".into(),
            function: FunctionDef {
                name: "get_weather".into(),
                description: "Get weather for a city".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "city": { "type": "string" } },
                    "required": ["city"]
                }),
            },
        }];
        let tooled = chat_completion(
            &cfg,
            vec![ChatMessage {
                role: Role::User,
                content: Some("Get the weather for Tokyo using the tool, then say done.".into()),
                tool_call_id: None,
                tool_calls: None,
            }],
            Some(tools),
            Some(512),
        )
        .await
        .expect("tool call");
        let msg = tooled.choices[0].message.as_ref().expect("message");
        assert!(
            msg.tool_calls.as_ref().map(|t| !t.is_empty()).unwrap_or(false),
            "expected a tool call in {:?}",
            tooled.choices[0].finish_reason
        );
        let tc = &msg.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tc.function.name, "get_weather");
    }
}
