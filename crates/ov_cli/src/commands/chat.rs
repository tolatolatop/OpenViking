//! Chat command for interacting with Vikingbot via OpenAPI
//!
//! Features:
//! - Proper line editing with rustyline (no ^[[D characters)
//! - Markdown rendering for bot responses
//! - Command history support
//! - Streaming response support

use std::time::Duration;

use clap::Parser;
use reqwest::Client;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use serde::{Deserialize, Serialize};
use termimad::MadSkin;

use crate::config::Config;
use crate::utils;

use crate::error::{Error, Result};

const DEFAULT_ENDPOINT: &str = "http://localhost:1933/bot/v1";
const HISTORY_FILE: &str = ".ov_chat_history";

/// Chat with Vikingbot via OpenAPI
#[derive(Debug, Parser)]
pub struct ChatCommand {
    /// API endpoint URL
    #[arg(short, long, default_value = DEFAULT_ENDPOINT)]
    pub endpoint: String,

    /// API key for authentication
    #[arg(short, long, env = "VIKINGBOT_API_KEY")]
    pub api_key: Option<String>,

    /// LLM provider name for this chat request
    #[arg(long)]
    pub provider: Option<String>,

    /// LLM model for this chat request
    #[arg(long)]
    pub model: Option<String>,

    /// LLM API base URL for this chat request
    #[arg(long)]
    pub api_base: Option<String>,

    /// LLM API key for this chat request
    #[arg(long)]
    pub model_api_key: Option<String>,

    /// Account identifier to send as X-OpenViking-Account
    #[arg(long)]
    pub account: Option<String>,

    /// User identifier to send as X-OpenViking-User
    #[arg(long)]
    pub user: Option<String>,

    /// Session ID to use (creates new if not provided)
    #[arg(short, long)]
    pub session: Option<String>,

    /// Sender ID
    #[arg(short, long, default_value = "user")]
    pub sender: String,

    /// Non-interactive mode (single message)
    #[arg(short, long)]
    pub message: Option<String>,

    /// Stream the response (default: true)
    #[arg(long, default_value_t = true)]
    pub stream: bool,

    /// Disable rich formatting / markdown rendering
    #[arg(long)]
    pub no_format: bool,

    /// Disable command history
    #[arg(long)]
    pub no_history: bool,
}

/// Chat message for API
#[derive(Debug, Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    content: String,
}

/// Request-scoped LLM overrides
#[derive(Debug, Serialize)]
struct RuntimeLlmOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
}

/// Chat request body
#[derive(Debug, Serialize)]
struct ChatRequest {
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<Vec<ChatMessage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime_llm: Option<RuntimeLlmOverrides>,
}

/// Chat response (non-streaming)
#[derive(Debug, Deserialize)]
struct ChatResponse {
    session_id: String,
    message: String,
    #[serde(default)]
    events: Option<Vec<serde_json::Value>>,
}

/// Stream event from SSE
#[derive(Debug, Deserialize)]
struct ChatStreamEvent {
    event: String, // "reasoning", "tool_call", "tool_result", "response"
    data: serde_json::Value,
    timestamp: Option<String>,
}

struct ChatAuth {
    api_key: Option<String>,
    account: Option<String>,
    user: Option<String>,
}

impl ChatCommand {
    /// Execute the chat command
    pub async fn execute(&self) -> Result<()> {
        let auth = self.resolve_auth()?;
        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| Error::Network(format!("Failed to create HTTP client: {}", e)))?;

        if let Some(message) = &self.message {
            // Single message mode
            self.send_message(&client, message, &auth).await
        } else {
            // Interactive mode
            self.run_interactive(&client, &auth).await
        }
    }

    fn resolve_auth(&self) -> Result<ChatAuth> {
        let config = Config::load()?;
        Ok(ChatAuth {
            api_key: self.api_key.clone().or(config.api_key),
            account: self.account.clone().or(config.account),
            user: self.user.clone().or(config.user),
        })
    }

    fn apply_auth_headers(
        &self,
        mut req_builder: reqwest::RequestBuilder,
        auth: &ChatAuth,
    ) -> reqwest::RequestBuilder {
        if let Some(api_key) = &auth.api_key {
            req_builder = req_builder.header("X-API-Key", api_key);
        }
        if let Some(account) = &auth.account {
            req_builder = req_builder.header("X-OpenViking-Account", account);
        }
        if let Some(user) = &auth.user {
            req_builder = req_builder.header("X-OpenViking-User", user);
        }
        req_builder
    }

    fn build_runtime_llm(&self) -> Option<RuntimeLlmOverrides> {
        let model = self
            .model
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())?
            .to_string();

        Some(RuntimeLlmOverrides {
            provider: self
                .provider
                .as_ref()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string()),
            model,
            api_base: self
                .api_base
                .as_ref()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string()),
            api_key: self
                .model_api_key
                .as_ref()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string()),
        })
    }

    fn build_request(
        &self,
        message: String,
        session_id: Option<String>,
        stream: bool,
    ) -> ChatRequest {
        let request = ChatRequest {
            message,
            session_id,
            user_id: Some(self.sender.clone()),
            stream,
            context: None,
            runtime_llm: self.build_runtime_llm(),
        };

        if let Some(runtime_llm) = &request.runtime_llm {
            eprintln!(
                "[ov chat] request-scoped LLM override enabled: provider={}, model={}, api_base={}, api_key={}",
                runtime_llm.provider.as_deref().unwrap_or("(auto)"),
                runtime_llm.model,
                runtime_llm.api_base.as_deref().unwrap_or("(default)"),
                if runtime_llm.api_key.is_some() { "set" } else { "unset" },
            );
        }

        request
    }

    /// Send a single message and get response
    async fn send_message(&self, client: &Client, message: &str, auth: &ChatAuth) -> Result<()> {
        if self.stream {
            self.send_message_stream(client, message, auth).await
        } else {
            self.send_message_non_stream(client, message, auth).await
        }
    }

    /// Send a single message with non-streaming response
    async fn send_message_non_stream(
        &self,
        client: &Client,
        message: &str,
        auth: &ChatAuth,
    ) -> Result<()> {
        let url = format!("{}/chat", self.endpoint);

        let request = self.build_request(message.to_string(), self.session.clone(), false);

        let req_builder = self.apply_auth_headers(client.post(&url).json(&request), auth);

        let response = req_builder
            .send()
            .await
            .map_err(|e| Error::Network(format!("Failed to send request: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Api(format!("Request failed ({}): {}", status, text)));
        }

        let chat_response: ChatResponse = response
            .json()
            .await
            .map_err(|e| Error::Parse(format!("Failed to parse response: {}", e)))?;

        // Print events if any
        self.print_events(&chat_response.events);

        // Print final response
        self.print_response(&chat_response.message);

        Ok(())
    }

    /// Send a single message with streaming response
    async fn send_message_stream(
        &self,
        client: &Client,
        message: &str,
        auth: &ChatAuth,
    ) -> Result<()> {
        let url = format!("{}/chat/stream", self.endpoint);

        let request = self.build_request(message.to_string(), self.session.clone(), true);

        let req_builder = self.apply_auth_headers(client.post(&url).json(&request), auth);

        let response = req_builder
            .send()
            .await
            .map_err(|e| Error::Network(format!("Failed to send request: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Api(format!("Request failed ({}): {}", status, text)));
        }

        // Process the SSE stream
        let mut response = response;
        let mut buffer = String::new();
        let mut final_message = String::new();

        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| Error::Network(format!("Stream error: {}", e)))?
        {
            let chunk_str = String::from_utf8_lossy(&chunk);
            buffer.push_str(&chunk_str);

            // Process complete lines from buffer
            while let Some(newline_pos) = buffer.find('\n') {
                let line = buffer[..newline_pos].trim_end().to_string();
                buffer = buffer[newline_pos + 1..].to_string();

                if line.is_empty() {
                    continue;
                }

                // Parse SSE line: "data: {json}"
                if let Some(data_str) = line.strip_prefix("data: ") {
                    if let Ok(event) = serde_json::from_str::<ChatStreamEvent>(data_str) {
                        self.print_stream_event(&event);
                        if event.event == "response" {
                            if let Some(msg) = event.data.as_str() {
                                final_message = msg.to_string();
                            } else if let Some(obj) = event.data.as_object() {
                                if let Some(msg) = obj.get("message").and_then(|m| m.as_str()) {
                                    final_message = msg.to_string();
                                } else if let Some(err) = obj.get("error").and_then(|e| e.as_str())
                                {
                                    eprintln!("\x1b[1;31mError: {}\x1b[0m", err);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Print final response with markdown if we have it
        if !final_message.is_empty() {
            println!();
            self.print_response(&final_message);
        }

        Ok(())
    }

    /// Run interactive chat mode with rustyline
    async fn run_interactive(&self, client: &Client, auth: &ChatAuth) -> Result<()> {
        println!("Vikingbot Chat - Interactive Mode");
        println!("Endpoint: {}", self.endpoint);
        if let Some(session) = &self.session {
            println!("Session: {}", session);
        }
        println!("Sender: {}", self.sender);
        println!("Type 'exit', 'quit', or press Ctrl+C to exit");
        println!("----------------------------------------\n");

        // Initialize rustyline editor
        let mut rl = DefaultEditor::new()
            .map_err(|e| Error::Client(format!("Failed to initialize editor: {}", e)))?;

        // Load history if enabled
        let history_path = if !self.no_history {
            self.get_history_path()
        } else {
            None
        };
        if let Some(ref path) = history_path {
            let _ = rl.load_history(path);
        }

        let mut session_id = self.session.clone();

        loop {
            // Read input with rustyline
            let prompt = "\x1b[1;32mYou:\x1b[0m ";
            match rl.readline(prompt) {
                Ok(line) => {
                    let input: &str = line.trim();

                    if input.is_empty() {
                        continue;
                    }

                    // Add to history
                    if !self.no_history {
                        let _ = rl.add_history_entry(input);
                    }

                    // Check for exit
                    if input.eq_ignore_ascii_case("exit") || input.eq_ignore_ascii_case("quit") {
                        println!("\nGoodbye!");
                        break;
                    }

                    // Send message
                    match self
                        .send_interactive_message(client, input, &mut session_id, auth)
                        .await
                    {
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("\x1b[1;31mError: {}\x1b[0m", e);
                        }
                    }
                }
                Err(ReadlineError::Interrupted) => {
                    // Ctrl+C
                    println!("\nGoodbye!");
                    break;
                }
                Err(ReadlineError::Eof) => {
                    // Ctrl+D
                    println!("\nGoodbye!");
                    break;
                }
                Err(e) => {
                    eprintln!("\x1b[1;31mError reading input: {}\x1b[0m", e);
                    break;
                }
            }
        }

        // Save history
        if let Some(ref path) = history_path {
            let _ = rl.save_history(path);
        }

        Ok(())
    }

    /// Send a message in interactive mode
    async fn send_interactive_message(
        &self,
        client: &Client,
        input: &str,
        session_id: &mut Option<String>,
        auth: &ChatAuth,
    ) -> Result<()> {
        if self.stream {
            self.send_interactive_message_stream(client, input, session_id, auth)
                .await
        } else {
            self.send_interactive_message_non_stream(client, input, session_id, auth)
                .await
        }
    }

    /// Send a message in interactive mode (non-streaming)
    async fn send_interactive_message_non_stream(
        &self,
        client: &Client,
        input: &str,
        session_id: &mut Option<String>,
        auth: &ChatAuth,
    ) -> Result<()> {
        let url = format!("{}/chat", self.endpoint);

        let request = self.build_request(input.to_string(), session_id.clone(), false);

        let req_builder = self.apply_auth_headers(client.post(&url).json(&request), auth);

        let response = req_builder
            .send()
            .await
            .map_err(|e| Error::Network(format!("Failed to send request: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Api(format!("Request failed ({}): {}", status, text)));
        }

        let chat_response: ChatResponse = response
            .json()
            .await
            .map_err(|e| Error::Parse(format!("Failed to parse response: {}", e)))?;

        // Save session ID
        if session_id.is_none() {
            *session_id = Some(chat_response.session_id.clone());
        }

        // Print events
        self.print_events(&chat_response.events);

        // Print response with markdown
        println!();
        self.print_response(&chat_response.message);
        println!();

        Ok(())
    }

    /// Send a message in interactive mode (streaming)
    async fn send_interactive_message_stream(
        &self,
        client: &Client,
        input: &str,
        session_id: &mut Option<String>,
        auth: &ChatAuth,
    ) -> Result<()> {
        let url = format!("{}/chat/stream", self.endpoint);

        let request = self.build_request(input.to_string(), session_id.clone(), true);

        let req_builder = self.apply_auth_headers(client.post(&url).json(&request), auth);

        let response = req_builder
            .send()
            .await
            .map_err(|e| Error::Network(format!("Failed to send request: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Api(format!("Request failed ({}): {}", status, text)));
        }

        // Process the SSE stream
        let mut response = response;
        let mut buffer = String::new();
        let mut final_message = String::new();
        let mut got_session_id = false;

        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| Error::Network(format!("Stream error: {}", e)))?
        {
            let chunk_str = String::from_utf8_lossy(&chunk);
            buffer.push_str(&chunk_str);

            // Process complete lines from buffer
            while let Some(newline_pos) = buffer.find('\n') {
                let line = buffer[..newline_pos].trim_end().to_string();
                buffer = buffer[newline_pos + 1..].to_string();

                if line.is_empty() {
                    continue;
                }

                // Parse SSE line: "data: {json}"
                if let Some(data_str) = line.strip_prefix("data: ") {
                    if let Ok(event) = serde_json::from_str::<ChatStreamEvent>(data_str) {
                        // Extract session_id from first response event if needed
                        if !got_session_id && session_id.is_none() {
                            if let Some(obj) = event.data.as_object() {
                                if let Some(sid) = obj.get("session_id").and_then(|s| s.as_str()) {
                                    *session_id = Some(sid.to_string());
                                    got_session_id = true;
                                }
                            }
                        }

                        self.print_stream_event(&event);
                        if event.event == "response" {
                            if let Some(msg) = event.data.as_str() {
                                final_message = msg.to_string();
                            } else if let Some(obj) = event.data.as_object() {
                                if let Some(msg) = obj.get("message").and_then(|m| m.as_str()) {
                                    final_message = msg.to_string();
                                } else if let Some(err) = obj.get("error").and_then(|e| e.as_str())
                                {
                                    eprintln!("\x1b[1;31mError: {}\x1b[0m", err);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Print final response with markdown
        if !final_message.is_empty() {
            println!();
            self.print_response(&final_message);
        }
        println!();

        Ok(())
    }

    /// Print a single stream event as it arrives
    fn print_stream_event(&self, event: &ChatStreamEvent) {
        if self.no_format {
            return;
        }

        match event.event.as_str() {
            "reasoning" => {
                if let Some(content) = event.data.as_str() {
                    println!(
                        "  \x1b[2mThink: {}...\x1b[0m",
                        utils::truncate_utf8(content, 200)
                    );
                }
            }
            "tool_call" => {
                if let Some(content) = event.data.as_str() {
                    Self::print_tool_call(content);
                }
            }
            "tool_result" => {
                if let Some(content) = event.data.as_str() {
                    let truncated = if content.len() > 300 {
                        format!("{}...", utils::truncate_utf8(content, 300))
                    } else {
                        content.to_string()
                    };
                    Self::print_tool_result(&truncated);
                }
            }
            "iteration" => {
                // Ignore iteration events for now
            }
            "response" => {
                // Response is handled separately
            }
            _ => {}
        }
    }

    /// Parse and print a tool_call with formatted styling
    fn print_tool_call(content: &str) {
        if let Some(paren_idx) = content.find('(') {
            let tool_name = &content[..paren_idx];
            let args = &content[paren_idx..];
            print!("  \x1b[2m├─ Calling: \x1b[0m");
            print!("\x1b[1m{}\x1b[0m", tool_name);
            println!("\x1b[2m{}\x1b[0m", args);
        } else {
            // Fallback if format doesn't match
            println!("  \x1b[2m├─ Calling: {}\x1b[0m", content);
        }
    }

    /// Print a tool_result with formatted styling
    fn print_tool_result(content: &str) {
        println!("  \x1b[2m└─ Result: {}\x1b[0m", content);
    }

    /// Print thinking/events (for non-streaming mode)
    fn print_events(&self, events: &Option<Vec<serde_json::Value>>) {
        if self.no_format {
            return;
        }

        if let Some(events) = events {
            for event in events {
                if let (Some(etype), Some(data)) = (
                    event.get("type").and_then(|v| v.as_str()),
                    event.get("data"),
                ) {
                    match etype {
                        "reasoning" => {
                            let content = data.as_str().unwrap_or("");
                            println!(
                                "  \x1b[2mThink: {}...\x1b[0m",
                                utils::truncate_utf8(content, 200)
                            );
                        }
                        "tool_call" => {
                            let content = data.as_str().unwrap_or("");
                            Self::print_tool_call(content);
                        }
                        "tool_result" => {
                            let content = data.as_str().unwrap_or("");
                            let truncated = if content.len() > 300 {
                                format!("{}...", utils::truncate_utf8(content, 300))
                            } else {
                                content.to_string()
                            };
                            Self::print_tool_result(&truncated);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Print response with optional markdown rendering
    fn print_response(&self, message: &str) {
        if self.no_format {
            println!("{}", message);
            return;
        }

        println!("\x1b[1;31mBot:\x1b[0m");

        // Try to render markdown, fall back to plain text
        render_markdown(message);
    }

    /// Get history file path
    fn get_history_path(&self) -> Option<std::path::PathBuf> {
        dirs::home_dir().map(|home| home.join(HISTORY_FILE))
    }
}

impl ChatCommand {
    /// Execute the chat command (public wrapper)
    pub async fn run(&self) -> Result<()> {
        self.execute().await
    }
}

#[allow(dead_code)]
impl ChatCommand {
    /// Create a new ChatCommand with the given parameters
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        endpoint: String,
        api_key: Option<String>,
        session: Option<String>,
        sender: String,
        message: Option<String>,
        stream: bool,
        no_format: bool,
        no_history: bool,
    ) -> Self {
        Self {
            endpoint,
            api_key,
            provider: None,
            model: None,
            api_base: None,
            model_api_key: None,
            account: None,
            user: None,
            session,
            sender,
            message,
            stream,
            no_format,
            no_history,
        }
    }
}

/// Render markdown to terminal using termimad
fn render_markdown(text: &str) {
    let skin = MadSkin::default();
    skin.print_text(text);
}

#[cfg(test)]
mod tests {
    use super::ChatCommand;

    fn base_command() -> ChatCommand {
        ChatCommand {
            endpoint: "http://localhost:1933/bot/v1".to_string(),
            api_key: Some("server-key".to_string()),
            provider: None,
            model: None,
            api_base: None,
            model_api_key: None,
            account: None,
            user: None,
            session: Some("session-1".to_string()),
            sender: "user".to_string(),
            message: None,
            stream: true,
            no_format: false,
            no_history: false,
        }
    }

    #[test]
    fn build_runtime_llm_uses_explicit_override_fields() {
        let mut cmd = base_command();
        cmd.provider = Some("openai".to_string());
        cmd.model = Some("gpt-4.1".to_string());
        cmd.api_base = Some("https://example.test/v1".to_string());
        cmd.model_api_key = Some("model-secret".to_string());

        let runtime_llm = cmd
            .build_runtime_llm()
            .expect("runtime_llm should be built when model is present");

        assert_eq!(runtime_llm.provider.as_deref(), Some("openai"));
        assert_eq!(runtime_llm.model, "gpt-4.1");
        assert_eq!(runtime_llm.api_base.as_deref(), Some("https://example.test/v1"));
        assert_eq!(runtime_llm.api_key.as_deref(), Some("model-secret"));
    }

    #[test]
    fn build_request_omits_runtime_llm_without_model() {
        let cmd = base_command();
        let request = cmd.build_request("hello".to_string(), cmd.session.clone(), false);
        let value = serde_json::to_value(&request).expect("request should serialize");

        assert!(value.get("runtime_llm").is_none());
        assert_eq!(value.get("message").and_then(|v| v.as_str()), Some("hello"));
    }

    #[test]
    fn build_request_includes_runtime_llm_when_model_is_present() {
        let mut cmd = base_command();
        cmd.provider = Some("openai".to_string());
        cmd.model = Some("gpt-4.1".to_string());
        cmd.api_base = Some("https://example.test/v1".to_string());
        cmd.model_api_key = Some("model-secret".to_string());

        let request = cmd.build_request("hello".to_string(), cmd.session.clone(), true);
        let value = serde_json::to_value(&request).expect("request should serialize");
        let runtime_llm = value
            .get("runtime_llm")
            .expect("runtime_llm should be present");

        assert_eq!(runtime_llm.get("provider").and_then(|v| v.as_str()), Some("openai"));
        assert_eq!(runtime_llm.get("model").and_then(|v| v.as_str()), Some("gpt-4.1"));
        assert_eq!(
            runtime_llm.get("api_base").and_then(|v| v.as_str()),
            Some("https://example.test/v1")
        );
        assert_eq!(
            runtime_llm.get("api_key").and_then(|v| v.as_str()),
            Some("model-secret")
        );
    }
}
