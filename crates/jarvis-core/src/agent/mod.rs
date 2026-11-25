//! The agent loop: model -> tool calls -> OS -> model, with a confirmation gate,
//! a cancellation switch and an audit trail.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::audit::Audit;
use crate::config::Config;
use crate::llm::{ChatMessage, Completion, LlmClient};
use crate::platform::Platform;
use crate::policy::{self, Verdict};
use crate::tools::{self, ToolContext};

/// Mirrors the island's visual states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Open,
    Working,
    Confirm,
    Done,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmRequest {
    pub id: String,
    pub tool: String,
    pub summary: String,
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Phase { phase: Phase, label: Option<String> },
    Delta(String),
    Confirm(ConfirmRequest),
    Final(String),
    Error(String),
}

pub type Emitter = Arc<dyn Fn(AgentEvent) + Send + Sync>;

/// Abstraction over the model so the loop can be tested without a network.
pub trait Completer: Send + Sync {
    fn complete<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        tools: &'a [Value],
        on_delta: &'a mut (dyn FnMut(&str) + Send),
    ) -> BoxFuture<'a, Result<Completion>>;
}

impl Completer for LlmClient {
    fn complete<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        tools: &'a [Value],
        on_delta: &'a mut (dyn FnMut(&str) + Send),
    ) -> BoxFuture<'a, Result<Completion>> {
        Box::pin(async move { LlmClient::complete(self, messages, tools, on_delta).await })
    }
}

struct Pending {
    id: String,
    reply: oneshot::Sender<bool>,
}

pub struct Agent {
    config: Config,
    completer: Arc<dyn Completer>,
    platform: Arc<dyn Platform>,
    audit: Audit,
    emit: Emitter,
    history: Mutex<Vec<ChatMessage>>,
    pending: Mutex<Option<Pending>>,
    cancelled: AtomicBool,
    confirm_timeout: Duration,
}

impl Agent {
    pub fn new(
        config: Config,
        platform: Arc<dyn Platform>,
        completer: Arc<dyn Completer>,
        audit: Audit,
        emit: Emitter,
    ) -> Self {
        Self {
            config,
            completer,
            platform,
            audit,
            emit,
            history: Mutex::new(Vec::new()),
            pending: Mutex::new(None),
            cancelled: AtomicBool::new(false),
            confirm_timeout: Duration::from_secs(10),
        }
    }

    /// Build the default agent: real model, real platform.
    pub fn from_config(
        config: Config,
        platform: Arc<dyn Platform>,
        audit: Audit,
        emit: Emitter,
    ) -> Self {
        let llm = Arc::new(LlmClient::new(&config));
        Self::new(config, platform, llm, audit, emit)
    }

    pub fn with_confirm_timeout(mut self, timeout: Duration) -> Self {
        self.confirm_timeout = timeout;
        self
    }

    pub fn history(&self) -> Vec<ChatMessage> {
        self.history
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub fn clear_history(&self) {
        if let Ok(mut guard) = self.history.lock() {
            guard.clear();
        }
    }

    /// Stop whatever is happening. The current loop returns at the next
    /// checkpoint and any pending confirmation is denied.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let taken = {
            let mut guard = match self.pending.lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
            guard.take()
        };
        if let Some(pending) = taken {
            let _ = pending.reply.send(false);
        }
    }

    /// Answer a confirmation request from the UI.
    pub fn confirm(&self, id: &str, allow: bool) {
        let taken = {
            let mut guard = match self.pending.lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
            let matches = guard
                .as_ref()
                .map(|pending| pending.id == id)
                .unwrap_or(false);
            if matches {
                guard.take()
            } else {
                None
            }
        };
        if let Some(pending) = taken {
            let _ = pending.reply.send(allow);
        }
    }

    pub async fn ask(&self, prompt: &str) -> Result<()> {
        self.cancelled.store(false, Ordering::SeqCst);

        self.push(ChatMessage::user(prompt.to_string()));
        self.phase(Phase::Working, Some("Thinking".to_string()));

        let schemas = tools::schema_values();
        let ctx = ToolContext {
            workspace: self.config.workspace.clone(),
            platform: self.platform.clone(),
        };

        for _step in 0..self.config.max_steps {
            if self.cancelled.load(Ordering::SeqCst) {
                self.emit(AgentEvent::Error("Cancelled.".to_string()));
                return Ok(());
            }

            let mut messages = vec![ChatMessage::system(system_prompt(
                &self.config,
                self.platform.name(),
            ))];
            messages.extend(self.snapshot());

            let mut on_delta = |text: &str| {
                self.emit(AgentEvent::Delta(text.to_string()));
            };

            let completion = match self
                .completer
                .complete(&messages, &schemas, &mut on_delta)
                .await
            {
                Ok(completion) => completion,
                Err(error) => {
                    self.emit(AgentEvent::Error(format!("{error:#}")));
                    return Err(error);
                }
            };
            if completion.tool_calls.is_empty() {
                let answer = if completion.content.trim().is_empty() {
                    "Done.".to_string()
                } else {
                    completion.content.clone()
                };
                self.push(ChatMessage::assistant(completion.content, Vec::new()));
                self.emit(AgentEvent::Final(answer));
                return Ok(());
            }

            self.push(ChatMessage::assistant(
                completion.content.clone(),
                completion.tool_calls.clone(),
            ));

            let mut images: Vec<String> = Vec::new();

            for call in &completion.tool_calls {
                let name = call.function.name.clone();
                let args = call.args();
                let summary = tools::summarise(&name, &args);
                let decision = policy::evaluate(&name, &args, &self.config.workspace);

                match decision.verdict {
                    Verdict::Blocked => {
                        self.audit
                            .record(&name, &args, &format!("blocked: {}", decision.reason));
                        self.push(ChatMessage::tool(
                            call.id.clone(),
                            format!(
                                "REFUSED by policy: {}. Do not retry; tell the user.",
                                decision.reason
                            ),
                        ));
                        continue;
                    }
                    Verdict::Confirm => {
                        if !self.request_permission(&name, &summary).await {
                            self.audit.record(&name, &args, "denied by user");
                            self.push(ChatMessage::tool(
                                call.id.clone(),
                                "The user denied this action. Do not retry it; ask what they would prefer.",
                            ));
                            continue;
                        }
                    }
                    Verdict::Safe => {}
                }

                if self.cancelled.load(Ordering::SeqCst) {
                    self.emit(AgentEvent::Error("Cancelled.".to_string()));
                    return Ok(());
                }

                self.phase(Phase::Working, Some(summary.clone()));

                match tools::execute(&name, &args, &ctx).await {
                    Ok(outcome) => {
                        self.audit.record(&name, &args, &outcome.summary);
                        if let Some(image) = outcome.image {
                            images.push(image);
                        }
                        self.push(ChatMessage::tool(call.id.clone(), outcome.text));
                    }
                    Err(error) => {
                        let message = format!("ERROR: {error:#}");
                        self.audit.record(&name, &args, &message);
                        self.push(ChatMessage::tool(call.id.clone(), message));
                    }
                }
            }

            for image in images {
                self.push(ChatMessage::user_with_image(
                    "Screenshot of the screen right now.",
                    &image,
                ));
            }
        }

        let message = format!(
            "I stopped after {} steps without finishing. Ask me to continue if you want me to keep going.",
            self.config.max_steps
        );
        self.emit(AgentEvent::Final(message));
        Ok(())
    }

    async fn request_permission(&self, tool: &str, summary: &str) -> bool {
        let id = format!("c-{}", chrono::Local::now().timestamp_micros());
        let (tx, rx) = oneshot::channel();

        {
            let mut guard = match self.pending.lock() {
                Ok(guard) => guard,
                Err(_) => return false,
            };
            *guard = Some(Pending {
                id: id.clone(),
                reply: tx,
            });
        }

        self.emit(AgentEvent::Confirm(ConfirmRequest {
            id: id.clone(),
            tool: tool.to_string(),
            summary: summary.to_string(),
        }));

        let allowed = match tokio::time::timeout(self.confirm_timeout, rx).await {
            Ok(Ok(allow)) => allow,
            // Timed out, or the sender was dropped: treat as "no".
            _ => false,
        };

        if let Ok(mut guard) = self.pending.lock() {
            if guard
                .as_ref()
                .map(|pending| pending.id == id)
                .unwrap_or(false)
            {
                *guard = None;
            }
        }

        if allowed {
            self.phase(Phase::Working, Some("Approved".to_string()));
        }
        allowed
    }

    fn push(&self, message: ChatMessage) {
        if let Ok(mut guard) = self.history.lock() {
            guard.push(message);
            trim(&mut guard);
        }
    }

    fn snapshot(&self) -> Vec<ChatMessage> {
        self.history
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    fn phase(&self, phase: Phase, label: Option<String>) {
        self.emit(AgentEvent::Phase { phase, label });
    }

    fn emit(&self, event: AgentEvent) {
        (self.emit)(event);
    }
}

/// Keep the window small and always starting at a user turn, so we never send a
/// tool result whose originating call was dropped.
fn trim(history: &mut Vec<ChatMessage>) {
    const MAX: usize = 24;
    if history.len() <= MAX {
        return;
    }
    let mut drop_count = history.len() - MAX;
    while drop_count < history.len() && history[drop_count].role != "user" {
        drop_count += 1;
    }
    history.drain(..drop_count);
}

fn system_prompt(config: &Config, platform: &str) -> String {
    format!(
        r#"You are Jarvis, the user's desktop assistant: you run on this machine and operate it for them.

Who you are:
- Calm, dry and efficient, like a good ship's computer. Competent, never flattering, never eager to please.
- You talk like a person, not a chatbot: no "Sure!", no "I'd be happy to", no restating the question, no filler.
- Candid. If something failed, say what failed and what you will try next. If you do not know, say so.

How long your answers are:
- One short sentence by default, two when a step needs explaining. That is the whole budget.
- No lists, headings or code blocks unless the user asks for detail or the answer is genuinely a list.
- Do not narrate what you are about to do. Do it, then report the outcome in a few words.
- When the user asks you to explain something, be as thorough as the question deserves.

What you can do:
You can see the screen (screenshot) and act: open and close applications, focus windows, type text, press key combinations, click, and read or write files in your workspace.

How to work:
- You are not guessing at a remote system: this is the user's live desktop. Take a screenshot whenever you are unsure what is on screen, before clicking, or when reading an error.
- Prefer a small, verifiable step, then look again. Do not fire a long blind sequence of clicks.
- Coordinates for click are physical screen pixels from the screenshot you took.
- To type into a specific app, focus it first, then type. Typing goes to whatever has focus.
- Only use tools you need. If the question is conversational, just answer.
- Some actions need the user's approval and will pause for it. If they deny one, do not retry it: ask what they would prefer instead.
- Never claim something happened unless a tool result confirms it.

Environment:
- OS: {platform}
- Workspace (the only place file tools may touch): {}
- Current date: {}"#,
        config.workspace.display(),
        chrono::Local::now().format("%Y-%m-%d")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{FunctionCall, ToolCall};
    use crate::platform::PlatformFuture;
    use std::collections::VecDeque;

    // ---------------------------------------------------------------- fakes

    struct FakePlatform {
        calls: Mutex<Vec<String>>,
    }

    impl FakePlatform {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
            })
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    macro_rules! record {
        ($self:ident, $name:expr, $value:expr) => {{
            $self
                .calls
                .lock()
                .unwrap()
                .push(format!("{}:{}", $name, $value));
            format!("{} ok", $name)
        }};
    }

    impl Platform for FakePlatform {
        fn name(&self) -> &'static str {
            "Fake"
        }
        fn open_app<'a>(&'a self, name: &'a str) -> PlatformFuture<'a> {
            Box::pin(async move { Ok(record!(self, "open_app", name)) })
        }
        fn close_app<'a>(&'a self, target: &'a str) -> PlatformFuture<'a> {
            Box::pin(async move { Ok(record!(self, "close_app", target)) })
        }
        fn focus_window<'a>(&'a self, target: &'a str) -> PlatformFuture<'a> {
            Box::pin(async move { Ok(record!(self, "focus_window", target)) })
        }
        fn list_windows(&self) -> PlatformFuture<'_> {
            Box::pin(async { Ok("Fake | Window".to_string()) })
        }
        fn type_text<'a>(&'a self, text: &'a str) -> PlatformFuture<'a> {
            Box::pin(async move { Ok(record!(self, "type_text", text)) })
        }
        fn press_keys<'a>(&'a self, combo: &'a str) -> PlatformFuture<'a> {
            Box::pin(async move { Ok(record!(self, "press_keys", combo)) })
        }
        fn click(&self, x: i32, y: i32) -> PlatformFuture<'_> {
            Box::pin(async move { Ok(record!(self, "click", format!("{x},{y}"))) })
        }
        fn screenshot(&self) -> PlatformFuture<'_> {
            Box::pin(async move {
                self.calls.lock().unwrap().push("screenshot".to_string());
                Ok("data:image/jpeg;base64,AAAA".to_string())
            })
        }
    }

    struct ScriptedCompleter {
        replies: Mutex<VecDeque<Completion>>,
        seen: Mutex<Vec<Vec<ChatMessage>>>,
    }

    impl ScriptedCompleter {
        fn new(replies: Vec<Completion>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.into()),
                seen: Mutex::new(Vec::new()),
            })
        }
        fn seen(&self) -> Vec<Vec<ChatMessage>> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Completer for ScriptedCompleter {
        fn complete<'a>(
            &'a self,
            messages: &'a [ChatMessage],
            _tools: &'a [Value],
            _on_delta: &'a mut (dyn FnMut(&str) + Send),
        ) -> BoxFuture<'a, Result<Completion>> {
            self.seen.lock().unwrap().push(messages.to_vec());
            let next = self.replies.lock().unwrap().pop_front();
            Box::pin(async move {
                next.ok_or_else(|| anyhow::anyhow!("scripted completer ran out of replies"))
            })
        }
    }

    fn call(id: &str, name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: args.to_string(),
            },
        }
    }

    fn agent(
        replies: Vec<Completion>,
        platform: Arc<FakePlatform>,
        confirm_timeout: Duration,
    ) -> (
        Arc<Agent>,
        Arc<ScriptedCompleter>,
        Arc<Mutex<Vec<AgentEvent>>>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        // Leak-free enough for a test: keep the dir alive by leaking it.
        let workspace = Box::leak(Box::new(dir)).path().to_path_buf();

        let config = Config::new(
            "https://example.test/v1",
            "test-key",
            "test-model",
            workspace,
        );
        let completer = ScriptedCompleter::new(replies);
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let emit: Emitter = Arc::new(move |event| sink.lock().unwrap().push(event));

        let agent = Agent::new(config, platform, completer.clone(), Audit::disabled(), emit)
            .with_confirm_timeout(confirm_timeout);

        (Arc::new(agent), completer, events)
    }

    // ---------------------------------------------------------------- tests

    #[tokio::test]
    async fn safe_tool_runs_and_the_answer_is_reported() {
        let platform = FakePlatform::new();
        let replies = vec![
            Completion {
                content: String::new(),
                tool_calls: vec![call("call_1", "screenshot", serde_json::json!({}))],
            },
            Completion {
                content: "There is a browser window open.".to_string(),
                tool_calls: Vec::new(),
            },
        ];

        let (agent, completer, events) = agent(replies, platform.clone(), Duration::from_secs(1));
        agent.ask("what is on my screen?").await.unwrap();

        assert_eq!(platform.calls(), vec!["screenshot".to_string()]);

        // The screenshot was attached as an image for the second turn.
        let seen = completer.seen();
        assert_eq!(seen.len(), 2);
        let last = seen.last().unwrap();
        let has_image = last.iter().any(|message| {
            message
                .content
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .any(|part| part.get("type").and_then(Value::as_str) == Some("image_url"))
                })
                .unwrap_or(false)
        });
        assert!(
            has_image,
            "screenshot was not attached to the follow-up turn"
        );

        let final_text = events
            .lock()
            .unwrap()
            .iter()
            .find_map(|event| match event {
                AgentEvent::Final(text) => Some(text.clone()),
                _ => None,
            })
            .expect("no final event");
        assert_eq!(final_text, "There is a browser window open.");
    }

    #[tokio::test]
    async fn denied_confirmation_never_reaches_the_os() {
        let platform = FakePlatform::new();
        let replies = vec![
            Completion {
                content: String::new(),
                tool_calls: vec![call(
                    "call_1",
                    "close_app",
                    serde_json::json!({ "target": "chrome" }),
                )],
            },
            Completion {
                content: "Okay, I left it open.".to_string(),
                tool_calls: Vec::new(),
            },
        ];

        // Tiny timeout: the confirmation auto-denies.
        let (agent, _completer, events) =
            agent(replies, platform.clone(), Duration::from_millis(20));

        agent.ask("close chrome").await.unwrap();

        assert!(
            platform.calls().is_empty(),
            "a denied action still hit the platform: {:?}",
            platform.calls()
        );

        let confirmations = events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, AgentEvent::Confirm(_)))
            .count();
        assert_eq!(confirmations, 1, "the user was never asked");
    }

    #[tokio::test]
    async fn approving_a_confirmation_runs_the_action() {
        let platform = FakePlatform::new();
        let replies = vec![
            Completion {
                content: String::new(),
                tool_calls: vec![call(
                    "call_1",
                    "open_app",
                    serde_json::json!({ "name": "Notepad" }),
                )],
            },
            Completion {
                content: "Notepad is open.".to_string(),
                tool_calls: Vec::new(),
            },
        ];

        let (agent, _completer, events) = agent(replies, platform.clone(), Duration::from_secs(2));

        // Approve as soon as the request appears.
        let watcher = {
            let agent = agent.clone();
            let events = events.clone();
            tokio::spawn(async move {
                for _ in 0..200 {
                    let id = events.lock().unwrap().iter().find_map(|event| match event {
                        AgentEvent::Confirm(request) => Some(request.id.clone()),
                        _ => None,
                    });
                    if let Some(id) = id {
                        agent.confirm(&id, true);
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
        };

        agent.ask("open notepad").await.unwrap();
        watcher.await.unwrap();

        assert_eq!(platform.calls(), vec!["open_app:Notepad".to_string()]);
    }

    #[tokio::test]
    async fn blocked_tools_are_refused_without_asking() {
        let platform = FakePlatform::new();
        let replies = vec![
            Completion {
                content: String::new(),
                tool_calls: vec![call(
                    "call_1",
                    "open_app",
                    serde_json::json!({ "name": "1Password" }),
                )],
            },
            Completion {
                content: "I won't touch that one.".to_string(),
                tool_calls: Vec::new(),
            },
        ];

        let (agent, completer, events) = agent(replies, platform.clone(), Duration::from_secs(1));
        agent.ask("open 1password").await.unwrap();

        assert!(platform.calls().is_empty());
        let asked = events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, AgentEvent::Confirm(_)));
        assert!(!asked, "blocked tools must not prompt for approval");

        // The refusal was passed back to the model.
        let seen = completer.seen();
        let last = seen.last().unwrap();
        assert!(last
            .iter()
            .any(|message| message.role == "tool" && message.text().contains("REFUSED")));
    }

    #[tokio::test]
    async fn cancelling_while_waiting_for_approval_stops_the_action() {
        let platform = FakePlatform::new();
        let replies = vec![Completion {
            content: String::new(),
            tool_calls: vec![call(
                "call_1",
                "close_app",
                serde_json::json!({ "target": "chrome" }),
            )],
        }];

        // Long timeout: only the Escape key (cancel) can end this wait.
        let (agent, _completer, events) = agent(replies, platform.clone(), Duration::from_secs(30));

        let runner = {
            let agent = agent.clone();
            tokio::spawn(async move { agent.ask("close chrome").await })
        };

        // Wait for the island to be asked, then hit cancel.
        for _ in 0..400 {
            let asked = events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, AgentEvent::Confirm(_)));
            if asked {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        agent.cancel();

        let _ = runner.await.unwrap();

        assert!(
            platform.calls().is_empty(),
            "cancel did not stop the action: {:?}",
            platform.calls()
        );
        let errored = events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, AgentEvent::Error(_)));
        assert!(errored, "no cancellation event reached the island");
    }

    #[test]
    fn history_trimming_starts_at_a_user_turn() {
        let mut history = Vec::new();
        for index in 0..40 {
            history.push(ChatMessage::user(format!("message {index}")));
            history.push(ChatMessage::assistant("ok".to_string(), Vec::new()));
        }
        trim(&mut history);
        assert_eq!(history.len(), 24, "the window stays at MAX");
        assert_eq!(history.first().unwrap().role, "user");
    }

    #[test]
    fn system_prompt_mentions_the_platform_and_workspace() {
        let config = Config::new("https://x.test/v1", "k", "m", "/tmp/jarvis-ws");
        let prompt = system_prompt(&config, "Windows");
        assert!(prompt.contains("Windows"));
        assert!(prompt.contains("/tmp/jarvis-ws"));
        assert!(prompt.contains("screenshot"));
        assert!(prompt.contains("desktop assistant"));
        assert!(prompt.to_lowercase().contains("one short sentence"));
    }
}
