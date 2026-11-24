//! Agent core for Jarvis: LLM client, tool calling loop, OS bridge and policy.
//!
//! Deliberately free of any UI dependency, so the whole thing can be built,
//! unit tested and reasoned about without a windowing system.

pub mod agent;
pub mod audit;
pub mod config;
pub mod llm;
pub mod platform;
pub mod policy;
pub mod tools;

pub use agent::{Agent, AgentEvent, Phase};
pub use audit::Audit;
pub use config::Config;
pub use platform::Platform;
