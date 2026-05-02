     1|//! Chat command for interacting with Vikingbot via OpenAPI
     2|//!
     3|//! Features:
     4|//! - Proper line editing with rustyline (no ^[[D characters)
     5|//! - Markdown rendering for bot responses
     6|//! - Command history support
     7|//! - Streaming response support
     8|
     9|use std::time::Duration;
    10|
    11|use clap::Parser;
    12|use reqwest::Client;
    13|use rustyline::DefaultEditor;
    14|use rustyline::error::ReadlineError;
    15|use serde::{Deserialize, Serialize};
    16|use termimad::MadSkin;
    17|
    18|use crate::config::Config;
    19|use crate::utils;
    20|
    21|use crate::error::{Error, Result};
    22|
    23|const DEFAULT_ENDPOINT: &str = "http://localhost:1933/bot/v1";
    24|const HISTORY_FILE: &str = ".ov_chat_history";
    25|
    26|/// Chat with Vikingbot via OpenAPI
    27|#[derive(Debug, Parser)]
    28|pub struct ChatCommand {
    29|    /// API endpoint URL
    30|    #[arg(short, long, default_value = DEFAULT_ENDPOINT)]
    31|    pub endpoint: String,
    32|
    33|    /// API key for authentication
    34|    #[arg(short, long, env = "VIKINGBOT_API_KEY")]
    35|    pub api_key: Option<String>,
    36|
    37|    /// LLM provider name for this chat request
    38|    #[arg(long)]
    39|    pub provider: Option<String>,
    40|
    41|    /// LLM model for this chat request
    42|    #[arg(long)]
    43|    pub model: Option<String>,
    44|
    45|    /// LLM API base URL for this chat request
    46|    #[arg(long)]
    47|    pub api_base: Option<String>,
    48|
    49|    /// LLM API key for this chat request
    50|    #[arg(long)]
    51|    pub model_api_key: Option<String>,
    52|
    53|    /// Account identifier to send as X-OpenViking-Account
    54|    #[arg(long)]
    55|    pub account: Option<String>,
    56|
    57|    /// User identifier to send as X-OpenViking-User
    58|    #[arg(long)]
    59|    pub user: Option<String>,
    60|
    61|    /// Session ID to use (creates new if not provided)
    62|    #[arg(short, long)]
    63|    pub session: Option<String>,
    64|
    65|    /// Sender ID
    66|    #[arg(long, default_value = "user")]
    67|    pub sender: String,
    68|
    69|    /// Non-interactive mode (single message)
    70|    #[arg(short, long)]
    71|    pub message: Option<String>,
    72|
    73|    /// Stream the response (default: true)
    74|    #[arg(long, default_value_t = true)]
    75|    pub stream: bool,
    76|
    77|    /// Disable rich formatting / markdown rendering
    78|    #[arg(long)]
    79|    pub no_format: bool,
    80|
    81|    /// Disable command history
    82|    #[arg(long)]
    83|    pub no_history: bool,
    84|}
    85|
    86|/// Chat message for API
    87|#[derive(Debug, Serialize, Deserialize)]
    88|struct ChatMessage {
    89|    role: String,
    90|    content: String,
    91|}
    92|
    93|/// Request-scoped LLM overrides
    94|#[derive(Debug, Serialize)]
    95|struct RuntimeLlmOverrides {
    96|    #[serde(skip_serializing_if = "Option::is_none")]
    97|    provider: Option<String>,
    98|    model: String,
    99|    #[serde(skip_serializing_if = "Option::is_none")]
   100|    api_base: Option<String>,
   101|    #[serde(skip_serializing_if = "Option::is_none")]
   102|    api_key: Option<String>,
   103|}
   104|
   105|/// Chat request body
   106|#[derive(Debug, Serialize)]
   107|struct ChatRequest {
   108|    message: String,
   109|    #[serde(skip_serializing_if = "Option::is_none")]
   110|    session_id: Option<String>,
   111|    #[serde(skip_serializing_if = "Option::is_none")]
   112|    user_id: Option<String>,
   113|    stream: bool,
   114|    #[serde(skip_serializing_if = "Option::is_none")]
   115|    context: Option<Vec<ChatMessage>>,
   116|    #[serde(skip_serializing_if = "Option::is_none")]
   117|    runtime_llm: Option<RuntimeLlmOverrides>,
   118|}
   119|
   120|/// Chat response (non-streaming)
   121|#[derive(Debug, Deserialize)]
   122|struct ChatResponse {
   123|    session_id: String,
   124|    message: String,
   125|    #[serde(default)]
   126|    response_id: Option<String>,
   127|    #[serde(default)]
   128|    events: Option<Vec<serde_json::Value>>,
   129|}
   130|
   131|/// Stream event from SSE
   132|#[derive(Debug, Deserialize)]
   133|struct ChatStreamEvent {
   134|    event: String, // "reasoning", "tool_call", "tool_result", "response"
   135|    data: serde_json::Value,
   136|    timestamp: Option<String>,
   137|}
   138|
   139|struct ChatAuth {
   140|    api_key: Option<String>,
   141|    account: Option<String>,
   142|    user: Option<String>,
   143|}
   144|
   145|impl ChatCommand {
   146|    /// Execute the chat command
   147|    pub async fn execute(&self) -> Result<()> {
   148|        let auth = self.resolve_auth()?;
   149|        let client = Client::builder()
   150|            .timeout(Duration::from_secs(300))
   151|            .build()
   152|            .map_err(|e| Error::Network(format!("Failed to create HTTP client: {}", e)))?;
   153|
   154|        if let Some(message) = &self.message {
   155|            // Single message mode
   156|            self.send_message(&client, message, &auth).await
   157|        } else {
   158|            // Interactive mode
   159|            self.run_interactive(&client, &auth).await
   160|        }
   161|    }
   162|
   163|    fn resolve_auth(&self) -> Result<ChatAuth> {
   164|        let config = Config::load()?;
   165|        Ok(ChatAuth {
   166|            api_key: self.api_key.clone().or(config.api_key),
   167|            account: self.account.clone().or(config.account),
   168|            user: self.user.clone().or(config.user),
   169|        })
   170|    }
   171|
   172|    fn apply_auth_headers(
   173|        &self,
   174|        mut req_builder: reqwest::RequestBuilder,
   175|        auth: &ChatAuth,
   176|    ) -> reqwest::RequestBuilder {
   177|        if let Some(api_key) = &auth.api_key {
   178|            req_builder = req_builder.header("X-API-Key", api_key);
   179|        }
   180|        if let Some(account) = &auth.account {
   181|            req_builder = req_builder.header("X-OpenViking-Account", account);
   182|        }
   183|        if let Some(user) = &auth.user {
   184|            req_builder = req_builder.header("X-OpenViking-User", user);
   185|        }
   186|        req_builder
   187|    }
   188|
   189|    fn build_runtime_llm(&self) -> Option<RuntimeLlmOverrides> {
   190|        let model = self
   191|            .model
   192|            .as_ref()
   193|            .map(|value| value.trim())
   194|            .filter(|value| !value.is_empty())?
   195|            .to_string();
   196|
   197|        Some(RuntimeLlmOverrides {
   198|            provider: self
   199|                .provider
   200|                .as_ref()
   201|                .map(|value| value.trim())
   202|                .filter(|value| !value.is_empty())
   203|                .map(|value| value.to_string()),
   204|            model,
   205|            api_base: self
   206|                .api_base
   207|                .as_ref()
   208|                .map(|value| value.trim())
   209|                .filter(|value| !value.is_empty())
   210|                .map(|value| value.to_string()),
   211|            api_key: self
   212|                .model_api_key
   213|                .as_ref()
   214|                .map(|value| value.trim())
   215|                .filter(|value| !value.is_empty())
   216|                .map(|value| value.to_string()),
   217|        })
   218|    }
   219|
   220|    fn build_request(
   221|        &self,
   222|        message: String,
   223|        session_id: Option<String>,
   224|        stream: bool,
   225|    ) -> ChatRequest {
   226|        let request = ChatRequest {
   227|            message,
   228|            session_id,
   229|            user_id: Some(self.sender.clone()),
   230|            stream,
   231|            context: None,
   232|            runtime_llm: self.build_runtime_llm(),
   233|        };
   234|
   235|        if let Some(runtime_llm) = &request.runtime_llm {
   236|            eprintln!(
   237|                "[ov chat] request-scoped LLM override enabled: provider={}, model={}, api_base={}, api_key={}",
   238|                runtime_llm.provider.as_deref().unwrap_or("(auto)"),
   239|                runtime_llm.model,
   240|                runtime_llm.api_base.as_deref().unwrap_or("(default)"),
   241|                if runtime_llm.api_key.is_some() { "set" } else { "unset" },
   242|            );
   243|        }
   244|
   245|        request
   246|    }
   247|
   248|    /// Send a single message and get response
   249|    async fn send_message(&self, client: &Client, message: &str, auth: &ChatAuth) -> Result<()> {
   250|        if self.stream {
   251|            self.send_message_stream(client, message, auth).await
   252|        } else {
   253|            self.send_message_non_stream(client, message, auth).await
   254|        }
   255|    }
   256|
   257|    /// Send a single message with non-streaming response
   258|    async fn send_message_non_stream(
   259|        &self,
   260|        client: &Client,
   261|        message: &str,
   262|        auth: &ChatAuth,
   263|    ) -> Result<()> {
   264|        let url = format!("{}/chat", self.endpoint);
   265|
   266|        let request = self.build_request(message.to_string(), self.session.clone(), false);
   267|
   268|        let req_builder = self.apply_auth_headers(client.post(&url).json(&request), auth);
   269|
   270|        let response = req_builder
   271|            .send()
   272|            .await
   273|            .map_err(|e| Error::Network(format!("Failed to send request: {}", e)))?;
   274|
   275|        if !response.status().is_success() {
   276|            let status = response.status();
   277|            let text = response.text().await.unwrap_or_default();
   278|            return Err(Error::Api(format!("Request failed ({}): {}", status, text)));
   279|        }
   280|
   281|        let chat_response: ChatResponse = response
   282|            .json()
   283|            .await
   284|            .map_err(|e| Error::Parse(format!("Failed to parse response: {}", e)))?;
   285|
   286|        // Print events if any
   287|        self.print_events(&chat_response.events);
   288|
   289|        // Print final response
   290|        self.print_response(&chat_response.message);
   291|
   292|        Ok(())
   293|    }
   294|
   295|    /// Send a single message with streaming response
   296|    async fn send_message_stream(
   297|        &self,
   298|        client: &Client,
   299|        message: &str,
   300|        auth: &ChatAuth,
   301|    ) -> Result<()> {
   302|        let url = format!("{}/chat/stream", self.endpoint);
   303|
   304|        let request = self.build_request(message.to_string(), self.session.clone(), true);
   305|
   306|        let req_builder = self.apply_auth_headers(client.post(&url).json(&request), auth);
   307|
   308|        let response = req_builder
   309|            .send()
   310|            .await
   311|            .map_err(|e| Error::Network(format!("Failed to send request: {}", e)))?;
   312|
   313|        if !response.status().is_success() {
   314|            let status = response.status();
   315|            let text = response.text().await.unwrap_or_default();
   316|            return Err(Error::Api(format!("Request failed ({}): {}", status, text)));
   317|        }
   318|
   319|        // Process the SSE stream
   320|        let mut response = response;
   321|        let mut buffer = String::new();
   322|        let mut final_message = String::new();
   323|        let mut response_id: Option<String> = None;
   324|
   325|        while let Some(chunk) = response
   326|            .chunk()
   327|            .await
   328|            .map_err(|e| Error::Network(format!("Stream error: {}", e)))?
   329|        {
   330|            let chunk_str = String::from_utf8_lossy(&chunk);
   331|            buffer.push_str(&chunk_str);
   332|
   333|            // Process complete lines from buffer
   334|            while let Some(newline_pos) = buffer.find('\n') {
   335|                let line = buffer[..newline_pos].trim_end().to_string();
   336|                buffer = buffer[newline_pos + 1..].to_string();
   337|
   338|                if line.is_empty() {
   339|                    continue;
   340|                }
   341|
   342|                // Parse SSE line: "data: {json}"
   343|                if let Some(data_str) = line.strip_prefix("data: ") {
   344|                    if let Ok(event) = serde_json::from_str::<ChatStreamEvent>(data_str) {
   345|                        self.print_stream_event(&event);
   346|                        if event.event == "response" {
   347|                            if let Some(msg) = event.data.as_str() {
   348|                                final_message = msg.to_string();
   349|                            } else if let Some(obj) = event.data.as_object() {
   350|                                if let Some(msg) = obj.get("content").and_then(|m| m.as_str()) {
   351|                                    final_message = msg.to_string();
   352|                                }
   353|                                if let Some(rid) = obj.get("response_id").and_then(|r| r.as_str()) {
   354|                                    response_id = Some(rid.to_string());
   355|                                }
   356|                                if let Some(err) = obj.get("error").and_then(|e| e.as_str()) {
   357|                                    eprintln!("\x1b[1;31mError: {}\x1b[0m", err);
   358|                                }
   359|                            }
   360|                        }
   361|                    }
   362|                }
   363|            }
   364|        }
   365|
   366|        if let Some(response_id) = response_id {
   367|            eprintln!("\x1b[2mResponse ID: {}\x1b[0m", response_id);
   368|        }
   369|
   370|        // Print final response with markdown if we have it
   371|        if !final_message.is_empty() {
   372|            println!();
   373|            self.print_response(&final_message);
   374|        }
   375|
   376|        Ok(())
   377|    }
   378|
   379|    /// Run interactive chat mode with rustyline
   380|    async fn run_interactive(&self, client: &Client, auth: &ChatAuth) -> Result<()> {
   381|        println!("Vikingbot Chat - Interactive Mode");
   382|        println!("Endpoint: {}", self.endpoint);
   383|        if let Some(session) = &self.session {
   384|            println!("Session: {}", session);
   385|        }
   386|        println!("Sender: {}", self.sender);
   387|        println!("Type 'exit', 'quit', or press Ctrl+C to exit");
   388|        println!("----------------------------------------\n");
   389|
   390|        // Initialize rustyline editor
   391|        let mut rl = DefaultEditor::new()
   392|            .map_err(|e| Error::Client(format!("Failed to initialize editor: {}", e)))?;
   393|
   394|        // Load history if enabled
   395|        let history_path = if !self.no_history {
   396|            self.get_history_path()
   397|        } else {
   398|            None
   399|        };
   400|        if let Some(ref path) = history_path {
   401|            let _ = rl.load_history(path);
   402|        }
   403|
   404|        let mut session_id = self.session.clone();
   405|
   406|        loop {
   407|            // Read input with rustyline
   408|            let prompt = "\x1b[1;32mYou:\x1b[0m ";
   409|            match rl.readline(prompt) {
   410|                Ok(line) => {
   411|                    let input: &str = line.trim();
   412|
   413|                    if input.is_empty() {
   414|                        continue;
   415|                    }
   416|
   417|                    // Add to history
   418|                    if !self.no_history {
   419|                        let _ = rl.add_history_entry(input);
   420|                    }
   421|
   422|                    // Check for exit
   423|                    if input.eq_ignore_ascii_case("exit") || input.eq_ignore_ascii_case("quit") {
   424|                        println!("\nGoodbye!");
   425|                        break;
   426|                    }
   427|
   428|                    // Send message
   429|                    match self
   430|                        .send_interactive_message(client, input, &mut session_id, auth)
   431|                        .await
   432|                    {
   433|                        Ok(_) => {}
   434|                        Err(e) => {
   435|                            eprintln!("\x1b[1;31mError: {}\x1b[0m", e);
   436|                        }
   437|                    }
   438|                }
   439|                Err(ReadlineError::Interrupted) => {
   440|                    // Ctrl+C
   441|                    println!("\nGoodbye!");
   442|                    break;
   443|                }
   444|                Err(ReadlineError::Eof) => {
   445|                    // Ctrl+D
   446|                    println!("\nGoodbye!");
   447|                    break;
   448|                }
   449|                Err(e) => {
   450|                    eprintln!("\x1b[1;31mError reading input: {}\x1b[0m", e);
   451|                    break;
   452|                }
   453|            }
   454|        }
   455|
   456|        // Save history
   457|        if let Some(ref path) = history_path {
   458|            let _ = rl.save_history(path);
   459|        }
   460|
   461|        Ok(())
   462|    }
   463|
   464|    /// Send a message in interactive mode
   465|    async fn send_interactive_message(
   466|        &self,
   467|        client: &Client,
   468|        input: &str,
   469|        session_id: &mut Option<String>,
   470|        auth: &ChatAuth,
   471|    ) -> Result<()> {
   472|        if self.stream {
   473|            self.send_interactive_message_stream(client, input, session_id, auth)
   474|                .await
   475|        } else {
   476|            self.send_interactive_message_non_stream(client, input, session_id, auth)
   477|                .await
   478|        }
   479|    }
   480|
   481|    /// Send a message in interactive mode (non-streaming)
   482|    async fn send_interactive_message_non_stream(
   483|        &self,
   484|        client: &Client,
   485|        input: &str,
   486|        session_id: &mut Option<String>,
   487|        auth: &ChatAuth,
   488|    ) -> Result<()> {
   489|        let url = format!("{}/chat", self.endpoint);
   490|
   491|        let request = self.build_request(input.to_string(), session_id.clone(), false);
   492|
   493|        let req_builder = self.apply_auth_headers(client.post(&url).json(&request), auth);
   494|
   495|        let response = req_builder
   496|            .send()
   497|            .await
   498|            .map_err(|e| Error::Network(format!("Failed to send request: {}", e)))?;
   499|
   500|        if !response.status().is_success() {
   501|            let status = response.status();
   502|            let text = response.text().await.unwrap_or_default();
   503|            return Err(Error::Api(format!("Request failed ({}): {}", status, text)));
   504|        }
   505|
   506|        let chat_response: ChatResponse = response
   507|            .json()
   508|            .await
   509|            .map_err(|e| Error::Parse(format!("Failed to parse response: {}", e)))?;
   510|
   511|        // Save session ID
   512|        if session_id.is_none() {
   513|            *session_id = Some(chat_response.session_id.clone());
   514|        }
   515|
   516|        // Print events
   517|        self.print_events(&chat_response.events);
   518|
   519|        // Print response with markdown
   520|        println!();
   521|        self.print_response(&chat_response.message);
   522|        println!();
   523|
   524|        Ok(())
   525|    }
   526|
   527|    /// Send a message in interactive mode (streaming)
   528|    async fn send_interactive_message_stream(
   529|        &self,
   530|        client: &Client,
   531|        input: &str,
   532|        session_id: &mut Option<String>,
   533|        auth: &ChatAuth,
   534|    ) -> Result<()> {
   535|        let url = format!("{}/chat/stream", self.endpoint);
   536|        let request_session_id = session_id.clone().or_else(|| self.session.clone());
   537|
   538|<<<<<<< HEAD
   539|        let request = ChatRequest {
   540|            message: input.to_string(),
   541|            session_id: request_session_id.clone(),
   542|            user_id: Some(self.sender.clone()),
   543|            stream: true,
   544|            context: None,
   545|        };
   546|=======
   547|        let request = self.build_request(input.to_string(), session_id.clone(), true);
   548|>>>>>>> 4ad42ac0 (feat(chat): support request-scoped llm overrides for ov chat)
   549|
   550|        let req_builder = self.apply_auth_headers(client.post(&url).json(&request), auth);
   551|
   552|        let response = req_builder
   553|            .send()
   554|            .await
   555|            .map_err(|e| Error::Network(format!("Failed to send request: {}", e)))?;
   556|
   557|        if !response.status().is_success() {
   558|            let status = response.status();
   559|            let text = response.text().await.unwrap_or_default();
   560|            return Err(Error::Api(format!("Request failed ({}): {}", status, text)));
   561|        }
   562|
   563|        let mut response = response;
   564|        let mut buffer = String::new();
   565|        let mut final_message = String::new();
   566|        let mut response_id: Option<String> = None;
   567|
   568|        if session_id.is_none() {
   569|            *session_id = request_session_id;
   570|        }
   571|
   572|        while let Some(chunk) = response
   573|            .chunk()
   574|            .await
   575|            .map_err(|e| Error::Network(format!("Stream error: {}", e)))?
   576|        {
   577|            let chunk_str = String::from_utf8_lossy(&chunk);
   578|            buffer.push_str(&chunk_str);
   579|
   580|            // Process complete lines from buffer
   581|            while let Some(newline_pos) = buffer.find('\n') {
   582|                let line = buffer[..newline_pos].trim_end().to_string();
   583|                buffer = buffer[newline_pos + 1..].to_string();
   584|
   585|                if line.is_empty() {
   586|                    continue;
   587|                }
   588|
   589|                // Parse SSE line: "data: {json}"
   590|                if let Some(data_str) = line.strip_prefix("data: ") {
   591|                    if let Ok(event) = serde_json::from_str::<ChatStreamEvent>(data_str) {
   592|                        self.print_stream_event(&event);
   593|                        if event.event == "response" {
   594|                            if let Some(msg) = event.data.as_str() {
   595|                                final_message = msg.to_string();
   596|                            } else if let Some(obj) = event.data.as_object() {
   597|                                if let Some(msg) = obj.get("content").and_then(|m| m.as_str()) {
   598|                                    final_message = msg.to_string();
   599|                                }
   600|                                if let Some(rid) = obj.get("response_id").and_then(|r| r.as_str()) {
   601|                                    response_id = Some(rid.to_string());
   602|                                }
   603|                                if let Some(err) = obj.get("error").and_then(|e| e.as_str()) {
   604|                                    eprintln!("\x1b[1;31mError: {}\x1b[0m", err);
   605|                                }
   606|                            }
   607|                        }
   608|                    }
   609|                }
   610|            }
   611|        }
   612|
   613|        if let Some(response_id) = response_id {
   614|            eprintln!("\x1b[2mResponse ID: {}\x1b[0m", response_id);
   615|        }
   616|
   617|        // Print final response with markdown
   618|        if !final_message.is_empty() {
   619|            println!();
   620|            self.print_response(&final_message);
   621|        }
   622|        println!();
   623|
   624|        Ok(())
   625|    }
   626|
   627|    /// Print a single stream event as it arrives
   628|    fn print_stream_event(&self, event: &ChatStreamEvent) {
   629|        if self.no_format {
   630|            return;
   631|        }
   632|
   633|        match event.event.as_str() {
   634|            "reasoning" => {
   635|                if let Some(content) = event.data.as_str() {
   636|                    println!(
   637|                        "  \x1b[2mThink: {}...\x1b[0m",
   638|                        utils::truncate_utf8(content, 200)
   639|                    );
   640|                }
   641|            }
   642|            "tool_call" => {
   643|                if let Some(content) = event.data.as_str() {
   644|                    Self::print_tool_call(content);
   645|                }
   646|            }
   647|            "tool_result" => {
   648|                if let Some(content) = event.data.as_str() {
   649|                    let truncated = if content.len() > 300 {
   650|                        format!("{}...", utils::truncate_utf8(content, 300))
   651|                    } else {
   652|                        content.to_string()
   653|                    };
   654|                    Self::print_tool_result(&truncated);
   655|                }
   656|            }
   657|            "iteration" => {
   658|                // Ignore iteration events for now
   659|            }
   660|            "response" => {
   661|                // Response is handled separately
   662|            }
   663|            _ => {}
   664|        }
   665|    }
   666|
   667|    /// Parse and print a tool_call with formatted styling
   668|    fn print_tool_call(content: &str) {
   669|        if let Some(paren_idx) = content.find('(') {
   670|            let tool_name = &content[..paren_idx];
   671|            let args = &content[paren_idx..];
   672|            print!("  \x1b[2m├─ Calling: \x1b[0m");
   673|            print!("\x1b[1m{}\x1b[0m", tool_name);
   674|            println!("\x1b[2m{}\x1b[0m", args);
   675|        } else {
   676|            // Fallback if format doesn't match
   677|            println!("  \x1b[2m├─ Calling: {}\x1b[0m", content);
   678|        }
   679|    }
   680|
   681|    /// Print a tool_result with formatted styling
   682|    fn print_tool_result(content: &str) {
   683|        println!("  \x1b[2m└─ Result: {}\x1b[0m", content);
   684|    }
   685|
   686|    /// Print thinking/events (for non-streaming mode)
   687|    fn print_events(&self, events: &Option<Vec<serde_json::Value>>) {
   688|        if self.no_format {
   689|            return;
   690|        }
   691|
   692|        if let Some(events) = events {
   693|            for event in events {
   694|                if let (Some(etype), Some(data)) = (
   695|                    event.get("type").and_then(|v| v.as_str()),
   696|                    event.get("data"),
   697|                ) {
   698|                    match etype {
   699|                        "reasoning" => {
   700|                            let content = data.as_str().unwrap_or("");
   701|                            println!(
   702|                                "  \x1b[2mThink: {}...\x1b[0m",
   703|                                utils::truncate_utf8(content, 200)
   704|                            );
   705|                        }
   706|                        "tool_call" => {
   707|                            let content = data.as_str().unwrap_or("");
   708|                            Self::print_tool_call(content);
   709|                        }
   710|                        "tool_result" => {
   711|                            let content = data.as_str().unwrap_or("");
   712|                            let truncated = if content.len() > 300 {
   713|                                format!("{}...", utils::truncate_utf8(content, 300))
   714|                            } else {
   715|                                content.to_string()
   716|                            };
   717|                            Self::print_tool_result(&truncated);
   718|                        }
   719|                        _ => {}
   720|                    }
   721|                }
   722|            }
   723|        }
   724|    }
   725|
   726|    /// Print response with optional markdown rendering
   727|    fn print_response(&self, message: &str) {
   728|        if self.no_format {
   729|            println!("{}", message);
   730|            return;
   731|        }
   732|
   733|        println!("\x1b[1;31mBot:\x1b[0m");
   734|
   735|        // Try to render markdown, fall back to plain text
   736|        render_markdown(message);
   737|    }
   738|
   739|    /// Get history file path
   740|    fn get_history_path(&self) -> Option<std::path::PathBuf> {
   741|        dirs::home_dir().map(|home| home.join(HISTORY_FILE))
   742|    }
   743|}
   744|
   745|impl ChatCommand {
   746|    /// Execute the chat command (public wrapper)
   747|    pub async fn run(&self) -> Result<()> {
   748|        self.execute().await
   749|    }
   750|}
   751|
   752|#[allow(dead_code)]
   753|impl ChatCommand {
   754|    /// Create a new ChatCommand with the given parameters
   755|    #[allow(clippy::too_many_arguments)]
   756|    pub fn new(
   757|        endpoint: String,
   758|        api_key: Option<String>,
   759|        session: Option<String>,
   760|        sender: String,
   761|        message: Option<String>,
   762|        stream: bool,
   763|        no_format: bool,
   764|        no_history: bool,
   765|    ) -> Self {
   766|        Self {
   767|            endpoint,
   768|            api_key,
   769|            provider: None,
   770|            model: None,
   771|            api_base: None,
   772|            model_api_key: None,
   773|            account: None,
   774|            user: None,
   775|            session,
   776|            sender,
   777|            message,
   778|            stream,
   779|            no_format,
   780|            no_history,
   781|        }
   782|    }
   783|}
   784|
   785|/// Render markdown to terminal using termimad
   786|fn render_markdown(text: &str) {
   787|    let skin = MadSkin::default();
   788|    skin.print_text(text);
   789|}
   790|
   791|#[cfg(test)]
   792|mod tests {
   793|    use super::ChatCommand;
   794|
   795|    fn base_command() -> ChatCommand {
   796|        ChatCommand {
   797|            endpoint: "http://localhost:1933/bot/v1".to_string(),
   798|            api_key: Some("server-key".to_string()),
   799|            provider: None,
   800|            model: None,
   801|            api_base: None,
   802|            model_api_key: None,
   803|            account: None,
   804|            user: None,
   805|            session: Some("session-1".to_string()),
   806|            sender: "user".to_string(),
   807|            message: None,
   808|            stream: true,
   809|            no_format: false,
   810|            no_history: false,
   811|        }
   812|    }
   813|
   814|    #[test]
   815|    fn build_runtime_llm_uses_explicit_override_fields() {
   816|        let mut cmd = base_command();
   817|        cmd.provider = Some("openai".to_string());
   818|        cmd.model = Some("gpt-4.1".to_string());
   819|        cmd.api_base = Some("https://example.test/v1".to_string());
   820|        cmd.model_api_key = Some("model-secret".to_string());
   821|
   822|        let runtime_llm = cmd
   823|            .build_runtime_llm()
   824|            .expect("runtime_llm should be built when model is present");
   825|
   826|        assert_eq!(runtime_llm.provider.as_deref(), Some("openai"));
   827|        assert_eq!(runtime_llm.model, "gpt-4.1");
   828|        assert_eq!(runtime_llm.api_base.as_deref(), Some("https://example.test/v1"));
   829|        assert_eq!(runtime_llm.api_key.as_deref(), Some("model-secret"));
   830|    }
   831|
   832|    #[test]
   833|    fn build_request_omits_runtime_llm_without_model() {
   834|        let cmd = base_command();
   835|        let request = cmd.build_request("hello".to_string(), cmd.session.clone(), false);
   836|        let value = serde_json::to_value(&request).expect("request should serialize");
   837|
   838|        assert!(value.get("runtime_llm").is_none());
   839|        assert_eq!(value.get("message").and_then(|v| v.as_str()), Some("hello"));
   840|    }
   841|
   842|    #[test]
   843|    fn build_request_includes_runtime_llm_when_model_is_present() {
   844|        let mut cmd = base_command();
   845|        cmd.provider = Some("openai".to_string());
   846|        cmd.model = Some("gpt-4.1".to_string());
   847|        cmd.api_base = Some("https://example.test/v1".to_string());
   848|        cmd.model_api_key = Some("model-secret".to_string());
   849|
   850|        let request = cmd.build_request("hello".to_string(), cmd.session.clone(), true);
   851|        let value = serde_json::to_value(&request).expect("request should serialize");
   852|        let runtime_llm = value
   853|            .get("runtime_llm")
   854|            .expect("runtime_llm should be present");
   855|
   856|        assert_eq!(runtime_llm.get("provider").and_then(|v| v.as_str()), Some("openai"));
   857|        assert_eq!(runtime_llm.get("model").and_then(|v| v.as_str()), Some("gpt-4.1"));
   858|        assert_eq!(
   859|            runtime_llm.get("api_base").and_then(|v| v.as_str()),
   860|            Some("https://example.test/v1")
   861|        );
   862|        assert_eq!(
   863|            runtime_llm.get("api_key").and_then(|v| v.as_str()),
   864|            Some("model-secret")
   865|        );
   866|    }
   867|}
   868|