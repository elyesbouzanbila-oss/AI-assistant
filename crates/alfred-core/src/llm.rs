use anyhow::{bail, Context, Result};
use base64::Engine as _;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Config;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionCall {
    pub name: String,
    /// JSON encoded as a string, exactly as the model streams it.
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

impl ToolCall {
    fn empty() -> Self {
        Self {
            id: String::new(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: String::new(),
                arguments: String::new(),
            },
        }
    }

    /// Arguments as JSON; falls back to an empty object when the model sent
    /// nothing (common for zero-arg tools).
    pub fn args(&self) -> Value {
        let raw = self.function.arguments.trim();
        if raw.is_empty() {
            return json!({});
        }
        serde_json::from_str(raw).unwrap_or_else(|_| json!({}))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    /// String, or an array of content parts when an image is attached.
    pub content: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(text: impl Into<String>) -> Self {
        Self::plain("system", text.into())
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::plain("user", text.into())
    }

    pub fn plain(role: &str, text: String) -> Self {
        Self {
            role: role.to_string(),
            content: Value::String(text),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn user_with_image(text: impl Into<String>, data_url: &str) -> Self {
        Self {
            role: "user".to_string(),
            content: json!([
                { "type": "text", "text": text.into() },
                { "type": "image_url", "image_url": { "url": data_url } }
            ]),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// Audio as an `input_audio` part, followed by the instruction. Models that
    /// accept audio (Gemini, the GPT audio models) transcribe or answer it.
    pub fn user_with_audio(instruction: impl Into<String>, audio_base64: &str) -> Self {
        Self {
            role: "user".to_string(),
            content: json!([
                {
                    "type": "input_audio",
                    "input_audio": { "data": audio_base64, "format": "wav" }
                },
                { "type": "text", "text": instruction.into() }
            ]),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant(content: String, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: Value::String(content),
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            tool_call_id: None,
        }
    }

    pub fn tool(call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: Value::String(text.into()),
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
        }
    }

    /// Best-effort text of this message, for history summaries and logs.
    pub fn text(&self) -> String {
        match &self.content {
            Value::String(text) => text.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" "),
            other => other.to_string(),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Completion {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
}

/// Accumulates streamed deltas. Kept separate from the HTTP layer so it can be
/// tested without a network.
#[derive(Debug, Default)]
struct Acc {
    content: String,
    calls: Vec<ToolCall>,
}

impl Acc {
    /// Feed one `data:` payload. Returns true when the stream signalled [DONE].
    fn feed(&mut self, data: &str, on_delta: &mut dyn FnMut(&str)) -> Result<bool> {
        let data = data.trim();
        if data.is_empty() {
            return Ok(false);
        }
        if data == "[DONE]" {
            return Ok(true);
        }

        let value: Value = match serde_json::from_str(data) {
            Ok(value) => value,
            // Keep-alive or provider-specific notice: not fatal.
            Err(_) => return Ok(false),
        };

        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown provider error");
            bail!("provider error: {message}");
        }

        let Some(choice) = value.get("choices").and_then(|c| c.get(0)) else {
            return Ok(false);
        };

        if let Some(text) = choice
            .get("delta")
            .and_then(|d| d.get("content"))
            .and_then(Value::as_str)
        {
            if !text.is_empty() {
                self.content.push_str(text);
                on_delta(text);
            }
        }

        if let Some(calls) = choice
            .get("delta")
            .and_then(|d| d.get("tool_calls"))
            .and_then(Value::as_array)
        {
            for fragment in calls {
                let index = fragment.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                while self.calls.len() <= index {
                    self.calls.push(ToolCall::empty());
                }
                let slot = &mut self.calls[index];

                if let Some(id) = fragment.get("id").and_then(Value::as_str) {
                    if !id.is_empty() {
                        slot.id = id.to_string();
                    }
                }
                if let Some(name) = fragment
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                {
                    slot.function.name.push_str(name);
                }
                if let Some(args) = fragment
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                {
                    slot.function.arguments.push_str(args);
                }
            }
        }

        Ok(false)
    }

    fn finish(mut self) -> Completion {
        // Providers differ on whether they send ids; we always need one to
        // reply with, so synthesise a stable fallback.
        for (index, call) in self.calls.iter_mut().enumerate() {
            if call.id.trim().is_empty() {
                call.id = format!("call_{index}");
            }
            if call.function.name.trim().is_empty() {
                call.function.name = String::new();
            }
        }
        self.calls
            .retain(|call| !call.function.name.trim().is_empty());

        Completion {
            content: self.content,
            tool_calls: self.calls,
        }
    }
}

/// Sent with the audio: without it models tend to answer the recording
/// instead of transcribing it.
const TRANSCRIBE_PROMPT: &str = "Transcribe the speech in this audio verbatim. \
Reply with the transcript only - no quotes, no commentary, no speaker labels. \
If nothing is intelligible, reply with nothing at all.";

pub struct LlmClient {
    http: reqwest::Client,
    chat: Target,
    /// Voice may live on a different endpoint and key than chat: a local model
    /// for conversation with cloud transcription, for instance.
    voice: Target,
    /// Local model tried when `chat` fails, so a rate limit or an outage is a
    /// slower answer instead of a dead assistant.
    fallback: Option<Target>,
    /// Runs before the second attempt: starts WSL and Ollama on a machine where
    /// nothing is listening yet. Injected by the shell, because booting an OS
    /// service is not the client's business.
    wake: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    /// A wake hook for the fallback only by default. A loopback *primary*
    /// needs one too, but a remote primary must not pay a wake-up retry.
    wake_primary: bool,
}

/// One endpoint, key and model. Chat and voice each have their own.
struct Target {
    url: String,
    api_key: String,
    model: String,
}

impl LlmClient {
    pub fn new(config: &Config) -> Self {
        // Attribution headers exist only for OpenRouter; other endpoints get an
        // empty map so a stray ALFRED_SITE_URL cannot leak to them.
        let mut headers = reqwest::header::HeaderMap::new();
        if config.is_openrouter() {
            insert(&mut headers, "X-OpenRouter-Title", "Alfred");
            if let Some(url) = valid_site_url(config.site_url.as_deref()) {
                insert(&mut headers, "HTTP-Referer", url);
            }
        }

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .unwrap_or_default();

        Self {
            http,
            chat: Target {
                url: config.chat_completions_url(),
                api_key: config.api_key.clone(),
                model: config.model.clone(),
            },
            voice: Target {
                url: config.transcription_url(),
                api_key: config.transcription_key().to_string(),
                model: config.transcription_model().to_string(),
            },
            fallback: None,
            wake: None,
            wake_primary: false,
        }
    }

    pub fn model(&self) -> &str {
        &self.chat.model
    }

    /// Retry a failed turn on a second endpoint instead of failing the turn.
    pub fn with_fallback(mut self, fallback: &crate::config::Fallback) -> Self {
        self.fallback = Some(Target {
            url: fallback.completions_url(),
            api_key: fallback.api_key.clone(),
            model: fallback.model.clone(),
        });
        self
    }

    /// Run before the second fallback attempt, e.g. to boot WSL and Ollama.
    pub fn with_wake(mut self, wake: std::sync::Arc<dyn Fn() + Send + Sync>) -> Self {
        self.wake = Some(wake);
        self
    }

    /// Also wake and retry once when the *primary* endpoint is local. Used when
    /// the user points chat itself at Ollama rather than using it as fallback.
    pub fn with_primary_wake(mut self, wake: std::sync::Arc<dyn Fn() + Send + Sync>) -> Self {
        self.wake = Some(wake);
        self.wake_primary = true;
        self
    }

    /// One streamed completion. `on_delta` receives text as it arrives so the
    /// island can show words appearing.
    pub async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Completion> {
        // Text that already reached the island cannot be unsaid, so a retry is
        // only safe before the first delta arrives.
        let streamed = std::sync::atomic::AtomicBool::new(false);
        let primary = {
            let streamed = &streamed;
            let mut sink = |text: &str| {
                streamed.store(true, std::sync::atomic::Ordering::SeqCst);
                on_delta(text);
            };
            self.request(&self.chat, 0.3, messages, tools, &mut sink)
                .await
        };

        let primary_error = match primary {
            Ok(completion) => return Ok(completion),
            Err(error) => error,
        };

        let Some(fallback) = &self.fallback else {
            // A local primary has no fallback to trigger its wake hook, so do
            // it here. Still one retry: start the service, then try once more.
            if self.wake_primary {
                if let Some(wake) = self.wake.clone() {
                    let wake_error = tokio::task::spawn_blocking(move || (wake)()).await.err();
                    return match self.request(&self.chat, 0.3, messages, tools, on_delta).await {
                        Ok(completion) => Ok(completion),
                        Err(error) => Err(match wake_error {
                            Some(wake_error) => error.context(format!(
                                "the local model could not be started: {wake_error}"
                            )),
                            None => error,
                        }),
                    };
                }
            }
            return Err(primary_error);
        };
        if streamed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(primary_error.context(format!(
                "{} stopped part-way through the answer, so the local fallback was not used",
                self.chat.model
            )));
        }

        // First try as-is: on a machine where Ollama is already serving this
        // costs nothing, and no wake-up delay is paid.
        let mut failure = match self.request(fallback, 0.3, messages, tools, on_delta).await {
            Ok(completion) => return Ok(completion),
            Err(error) => error,
        };

        if let Some(wake) = self.wake.clone() {
            // Blocking work (wsl.exe, waiting for the port) off the async runtime.
            let wake_error = tokio::task::spawn_blocking(move || (wake)()).await.err();
            match self.request(fallback, 0.3, messages, tools, on_delta).await {
                Ok(completion) => return Ok(completion),
                Err(error) => {
                    failure = match wake_error {
                        Some(wake_error) => error.context(format!(
                            "the local model could not be started: {wake_error}"
                        )),
                        None => error,
                    }
                }
            }
        }

        Err(failure.context(format!(
            "the primary model {} failed first: {primary_error:#}",
            self.chat.model
        )))
    }

    /// The models this endpoint advertises, used by the settings panel to test a
    /// configuration before saving it. `/models` costs no tokens and proves the
    /// URL, the network path and the key in one round trip.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let url = format!(
            "{}/models",
            self.chat.url.trim_end_matches("/chat/completions")
        );

        let mut request = self
            .http
            .get(&url)
            .timeout(std::time::Duration::from_secs(20));
        if !self.chat.api_key.trim().is_empty() {
            request = request.bearer_auth(&self.chat.api_key);
        }

        let response = request
            .send()
            .await
            .with_context(|| format!("could not reach {url}"))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let snippet: String = body.chars().take(200).collect();
            bail!("{url} returned {status}: {snippet}");
        }

        let value: Value = serde_json::from_str(&body)
            .with_context(|| format!("{url} did not return JSON"))?;
        let models = value
            .get("data")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("id").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Ok(models)
    }

    /// Transcribe a WAV recording with the model's audio input, so voice needs
    /// no second service or key: the endpoint that answers also listens.
    pub async fn transcribe(&self, wav: &[u8]) -> Result<String> {
        let audio = base64::engine::general_purpose::STANDARD.encode(wav);
        let message = ChatMessage::user_with_audio(TRANSCRIBE_PROMPT, &audio);
        let mut sink = |_: &str| {};

        let completion = self
            .request(&self.voice, 0.0, &[message], &[], &mut sink)
            .await
            .with_context(|| {
                format!(
                    "voice transcription failed with {} at {}. If that model cannot take \
                     audio input, set ALFRED_STT_MODEL to one that can",
                    self.voice.model, self.voice.url
                )
            })?;

        let text = completion
            .content
            .trim()
            .trim_matches('"')
            .trim()
            .to_string();
        if text.is_empty() {
            bail!("no speech was recognised in that recording");
        }
        Ok(text)
    }

    async fn request(
        &self,
        target: &Target,
        temperature: f32,
        messages: &[ChatMessage],
        tools: &[Value],
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Completion> {
        let mut body = json!({
            "model": target.model,
            "messages": messages,
            "stream": true,
            "temperature": temperature,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.to_vec());
            body["tool_choice"] = Value::String("auto".to_string());
        }

        let mut request = self.http.post(&target.url);
        // A loopback server such as Ollama takes no credentials, and an empty
        // bearer token is worse than sending none at all.
        if !target.api_key.trim().is_empty() {
            request = request.bearer_auth(&target.api_key);
        }
        let response = request
            .json(&body)
            .send()
            .await
            .with_context(|| format!("could not reach {}", target.url))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(400).collect();
            bail!(
                "model endpoint returned {status}. Check ALFRED_BASE_URL, ALFRED_MODEL ({}), \
                 and that the key is valid. Response: {snippet}",
                target.model
            );
        }

        let is_sse = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.contains("event-stream"))
            .unwrap_or(false);

        // Some servers ignore `stream: true` and answer with one JSON body.
        if !is_sse {
            let value: Value = response.json().await.context("invalid JSON response")?;
            return Self::from_whole_body(&value, on_delta);
        }

        let mut stream = response.bytes_stream();
        let mut buffer: Vec<u8> = Vec::new();
        let mut acc = Acc::default();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("stream interrupted")?;
            buffer.extend_from_slice(&chunk);

            for line in take_lines(&mut buffer) {
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                if acc.feed(payload, on_delta)? {
                    return Ok(acc.finish());
                }
            }
        }

        Ok(acc.finish())
    }

    fn from_whole_body(value: &Value, on_delta: &mut dyn FnMut(&str)) -> Result<Completion> {
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown provider error");
            bail!("provider error: {message}");
        }

        let message = value
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .cloned()
            .unwrap_or(Value::Null);

        let content = message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !content.is_empty() {
            on_delta(&content);
        }

        let tool_calls = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|calls| {
                calls
                    .iter()
                    .filter_map(|call| serde_json::from_value::<ToolCall>(call.clone()).ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Ok(Completion {
            content,
            tool_calls,
        })
    }
}

/// Take the complete lines out of `buffer`, leaving a partial tail behind.
///
/// Works on bytes: a chunk boundary can split a multi-byte character, and
/// decoding each chunk on its own turns that character into U+FFFD.
fn take_lines(buffer: &mut Vec<u8>) -> Vec<String> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, byte) in buffer.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let mut line = String::from_utf8_lossy(&buffer[start..index]).into_owned();
        if line.ends_with('\r') {
            line.pop();
        }
        lines.push(line);
        start = index + 1;
    }
    if start > 0 {
        buffer.drain(..start);
    }
    lines
}

/// A site URL is only usable as `HTTP-Referer` when it parses and has an http(s)
/// host; anything else is ignored rather than sent as a broken header.
fn valid_site_url(site_url: Option<&str>) -> Option<&str> {
    let url = site_url?.trim();
    match url::Url::parse(url) {
        Ok(parsed)
            if matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some() =>
        {
            Some(url)
        }
        _ => None,
    }
}

fn insert(headers: &mut reqwest::header::HeaderMap, name: &'static str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        reqwest::header::HeaderName::try_from(name),
        reqwest::header::HeaderValue::from_str(value),
    ) {
        headers.insert(name, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribution_headers_only_go_to_openrouter() {
        // No OpenRouter headers are built for any other endpoint, whatever
        // ALFRED_SITE_URL holds.
        let other = Config::new("https://api.openai.com/v1", "k", "m", "/tmp");
        assert!(!other.is_openrouter());

        let openrouter = Config::new("https://openrouter.ai/api/v1", "k", "m", "/tmp");
        assert!(openrouter.is_openrouter());

        // Lookalike hosts are not OpenRouter.
        let lookalike = Config::new("https://openrouter.ai.example.com/v1", "k", "m", "/tmp");
        assert!(!lookalike.is_openrouter());
    }

    #[test]
    fn only_valid_site_urls_are_forwarded() {
        assert_eq!(valid_site_url(None), None);
        assert_eq!(valid_site_url(Some("/relative")), None);
        assert_eq!(valid_site_url(Some("ftp://x.test")), None);
        assert_eq!(
            valid_site_url(Some("https://x.test")),
            Some("https://x.test")
        );
    }

    #[test]
    fn assembles_fragmented_tool_arguments() {
        let mut acc = Acc::default();
        let mut sink = |_: &str| {};

        let lines = [
            r#"{"choices":[{"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"open_app","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"name\""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"Notepad\"}"}}]}}]}"#,
        ];
        for line in lines {
            assert!(!acc.feed(line, &mut sink).unwrap());
        }

        let completion = acc.finish();
        assert_eq!(completion.tool_calls.len(), 1);
        let call = &completion.tool_calls[0];
        assert_eq!(call.id, "call_abc");
        assert_eq!(call.function.name, "open_app");
        assert_eq!(call.args(), json!({ "name": "Notepad" }));
    }

    #[test]
    fn streams_text_and_stops_on_done() {
        let mut acc = Acc::default();
        let mut seen = String::new();
        let mut sink = |text: &str| seen.push_str(text);

        acc.feed(r#"{"choices":[{"delta":{"content":"Hel"}}]}"#, &mut sink)
            .unwrap();
        acc.feed(r#"{"choices":[{"delta":{"content":"lo"}}]}"#, &mut sink)
            .unwrap();
        assert!(acc.feed("[DONE]", &mut sink).unwrap());

        let completion = acc.finish();
        assert_eq!(completion.content, "Hello");
        assert_eq!(seen, "Hello");
        assert!(completion.tool_calls.is_empty());
    }

    #[test]
    fn lines_split_across_chunks_keep_their_utf8() {
        let payload = "data: {\"choices\":[{\"delta\":{\"content\":\"héllo \u{1F680}\"}}]}\n\n";
        let bytes = payload.as_bytes();
        // Split in the middle of the four-byte emoji: decoding that first chunk
        // on its own is exactly what used to lose it.
        let split = payload.find('\u{1F680}').unwrap() + 2;
        assert!(!String::from_utf8_lossy(&bytes[..split]).contains('\u{1F680}'));

        let mut buffer = Vec::new();
        buffer.extend_from_slice(&bytes[..split]);
        assert!(
            take_lines(&mut buffer).is_empty(),
            "a partial line must wait for the rest"
        );

        buffer.extend_from_slice(&bytes[split..]);
        let lines = take_lines(&mut buffer);
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            "data: {\"choices\":[{\"delta\":{\"content\":\"héllo \u{1F680}\"}}]}"
        );
        assert!(buffer.is_empty());
    }

    /// A one-shot HTTP server, so the fallback path is tested against real
    /// sockets instead of a stub of the client itself.
    fn one_shot(
        status: u16,
        content_type: &'static str,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut scratch = [0u8; 4096];
                let _ = stream.read(&mut scratch);
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            }
        });
        (format!("http://127.0.0.1:{port}/v1"), handle)
    }

    /// A server that answers the first request with an error and the second
    /// with a streamed completion, so a wake-and-retry can be tested for real.
    fn fail_then_answer() -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let replies = [
                (500, "application/json", r#"{"error":{"message":"not up yet"}}"#),
                (
                    200,
                    "text/event-stream",
                    "data: {\"choices\":[{\"delta\":{\"content\":\"local answer\"}}]}\n\ndata: [DONE]\n\n",
                ),
            ];
            for (status, content_type, body) in replies {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut scratch = [0u8; 4096];
                    let _ = stream.read(&mut scratch);
                    let head = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(head.as_bytes());
                }
            }
        });
        (format!("http://127.0.0.1:{port}/v1"), handle)
    }

    /// A port nothing listens on, obtained by binding and immediately releasing.
    fn dead_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    #[tokio::test]
    async fn list_models_reads_the_ids_the_endpoint_advertises() {
        let (base, handle) = one_shot(
            200,
            "application/json",
            r#"{"data":[{"id":"alpha"},{"id":"beta"},{"missing":"id"}]}"#,
        );

        let config = Config::new(base, "sk-test", "alpha", "/tmp/ws");
        let models = LlmClient::new(&config)
            .list_models()
            .await
            .expect("the endpoint answered");

        assert_eq!(models, vec!["alpha".to_string(), "beta".to_string()]);
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn list_models_reports_a_rejected_key() {
        let (base, handle) = one_shot(
            401,
            "application/json",
            r#"{"error":{"message":"invalid api key"}}"#,
        );

        let config = Config::new(base, "sk-wrong", "alpha", "/tmp/ws");
        let error = LlmClient::new(&config)
            .list_models()
            .await
            .expect_err("a rejected key is not success");

        let message = format!("{error:#}");
        assert!(message.contains("401"), "{message}");
        assert!(message.contains("invalid api key"), "{message}");
        assert!(!message.contains("sk-wrong"), "the key must not appear: {message}");
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn a_failing_primary_retries_on_the_fallback_model() {
        let (primary, primary_handle) = one_shot(
            500,
            "application/json",
            r#"{"error":{"message":"provider is down"}}"#,
        );
        let (local, local_handle) = one_shot(
            200,
            "text/event-stream",
            "data: {\"choices\":[{\"delta\":{\"content\":\"local answer\"}}]}\n\ndata: [DONE]\n\n",
        );

        let config = Config::new(primary, "sk-cloud", "cloud-model", "/tmp/ws");
        let fallback = crate::config::Fallback {
            base_url: local,
            api_key: String::new(),
            model: "minicpm5:latest".into(),
        };

        let mut heard = String::new();
        let completion = LlmClient::new(&config)
            .with_fallback(&fallback)
            .complete(&[ChatMessage::user("hello")], &[], &mut |text| {
                heard.push_str(text);
            })
            .await
            .expect("the fallback answered");

        assert_eq!(completion.content, "local answer");
        assert_eq!(heard, "local answer", "the island still sees it stream");
        primary_handle.join().unwrap();
        local_handle.join().unwrap();
    }

    #[tokio::test]
    async fn a_local_primary_is_woken_and_retried_once() {
        let (url, handle) = fail_then_answer();
        let config = Config::new(url, "", "local-model", "/tmp/ws");
        let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wake = {
            let wakes = wakes.clone();
            std::sync::Arc::new(move || {
                wakes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
        };

        let completion = LlmClient::new(&config)
            .with_primary_wake(wake)
            .complete(&[ChatMessage::user("hello")], &[], &mut |_| {})
            .await
            .expect("the local model answered after waking");

        assert_eq!(completion.content, "local answer");
        assert_eq!(
            wakes.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the local model is started exactly once"
        );
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn a_dead_fallback_starts_the_local_model_once_then_reports_both() {
        let (primary, handle) = one_shot(
            503,
            "application/json",
            r#"{"error":{"message":"no capacity"}}"#,
        );
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let config = Config::new(primary, "sk-cloud", "cloud-model", "/tmp/ws");
        let fallback = crate::config::Fallback {
            base_url: format!("http://127.0.0.1:{}/v1", dead_port()),
            api_key: String::new(),
            model: "minicpm5:latest".into(),
        };
        let wake: std::sync::Arc<dyn Fn() + Send + Sync> = {
            let calls = calls.clone();
            std::sync::Arc::new(move || {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
        };

        let error = LlmClient::new(&config)
            .with_fallback(&fallback)
            .with_wake(wake)
            .complete(&[ChatMessage::user("hello")], &[], &mut |_| {})
            .await
            .expect_err("both endpoints are down");

        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the local model is started exactly once, not per attempt"
        );
        let message = format!("{error:#}");
        assert!(
            message.contains("cloud-model"),
            "the primary failure is named too: {message}"
        );
        handle.join().unwrap();
    }

    #[test]
    fn ignores_keepalives_and_reports_provider_errors() {
        let mut acc = Acc::default();
        let mut sink = |_: &str| {};
        assert!(!acc.feed("", &mut sink).unwrap());
        assert!(!acc.feed(": ping", &mut sink).unwrap());
        assert!(acc
            .feed(r#"{"error":{"message":"bad key"}}"#, &mut sink)
            .is_err());
    }

    #[test]
    fn synthesises_missing_tool_call_ids() {
        let mut acc = Acc::default();
        let mut sink = |_: &str| {};
        acc.feed(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"screenshot","arguments":"{}"}}]}}]}"#,
            &mut sink,
        )
        .unwrap();

        let completion = acc.finish();
        assert_eq!(completion.tool_calls[0].id, "call_0");
    }

    #[test]
    fn image_messages_use_content_parts() {
        let message = ChatMessage::user_with_image("what is this?", "data:image/jpeg;base64,AAA");
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["content"][0]["type"], "text");
        assert_eq!(json["content"][1]["type"], "image_url");
        assert_eq!(message.text(), "what is this?");
    }

    #[test]
    fn audio_messages_use_the_input_audio_part() {
        let message = ChatMessage::user_with_audio("transcribe", "AAAA");
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["content"][0]["type"], "input_audio");
        assert_eq!(json["content"][0]["input_audio"]["format"], "wav");
        assert_eq!(json["content"][0]["input_audio"]["data"], "AAAA");
        assert_eq!(json["content"][1]["type"], "text");
        assert_eq!(message.text(), "transcribe");
    }

    #[test]
    fn tool_messages_carry_the_call_id() {
        let message = ChatMessage::tool("call_1", "ok");
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["role"], "tool");
        assert_eq!(json["tool_call_id"], "call_1");
        assert!(json.get("tool_calls").is_none());
    }
}
