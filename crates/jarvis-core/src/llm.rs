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
        // empty map so a stray JARVIS_SITE_URL cannot leak to them.
        let mut headers = reqwest::header::HeaderMap::new();
        if config.is_openrouter() {
            insert(&mut headers, "X-OpenRouter-Title", "Jarvis");
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
        }
    }

    pub fn model(&self) -> &str {
        &self.chat.model
    }

    /// One streamed completion. `on_delta` receives text as it arrives so the
    /// island can show words appearing.
    pub async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Completion> {
        self.request(&self.chat, 0.3, messages, tools, on_delta)
            .await
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
                     audio input, set JARVIS_STT_MODEL to one that can",
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
                "model endpoint returned {status}. Check JARVIS_BASE_URL, JARVIS_MODEL ({}), \
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
        // JARVIS_SITE_URL holds.
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
