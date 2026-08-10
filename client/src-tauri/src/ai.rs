//! AI client — OpenAI-compatible chat completions with tool/function calling.
//!
//! Config (base_url, api_key, model) lives in `~/.config/puppetterm/ai.json`
//! (outside the repo) or in the PUPPETTERM_AI_* environment variables. The API
//! key never crosses into the frontend.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// AI provider config, stored on disk (never committed).
#[derive(Serialize, Deserialize, Clone)]
pub struct AiConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

/// Non-secret view of the config for the settings UI.
#[derive(Serialize)]
pub struct AiConfigView {
    pub base_url: String,
    pub model: String,
    pub has_api_key: bool,
}

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
    if let (Some(base_url), Some(api_key), Some(model)) = (&env_base, &env_key, &env_model) {
        return Ok(AiConfig { base_url: base_url.clone(), api_key: api_key.clone(), model: model.clone() });
    }
    let data = std::fs::read_to_string(config_path())
        .map_err(|e| format!("AI config not found: {e} (set PUPPETTERM_AI_* or create {})", config_path().display()))?;
    serde_json::from_str(&data).map_err(|e| format!("invalid AI config: {e}"))
}

pub fn save_config(cfg: &AiConfig) -> Result<(), String> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let data = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
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

/// Call the OpenAI-compatible chat completions endpoint.
pub async fn chat_completion(
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

#[cfg(test)]
mod tests {
    use super::*;

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
