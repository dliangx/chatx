//! Conversation state + prompt rendering for a local LLM.
//!
//! A [`Conversation`] is a system prompt plus a growing list of [`Message`]s.
//! It renders the whole exchange into ONE prompt string in a simple, stable
//! chat format, and can trim its own history to fit a token budget — the
//! safety valve for local models with a small `n_ctx`.

use serde::{Deserialize, Serialize};

/// Who produced a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    fn tag(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// One turn in the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub text: String,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self { role: Role::User, text: text.into() }
    }
    pub fn assistant(text: impl Into<String>) -> Self {
        Self { role: Role::Assistant, text: text.into() }
    }
}

/// Rough characters-per-token constant for budgeting. BPE on mixed CJK+ASCII
/// text lands near this; we only need a conservative estimate, not exactness.
const CHARS_PER_TOKEN: usize = 3;

/// A running conversation with a local model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    system: String,
    history: Vec<Message>,
}

impl Conversation {
    /// Start a conversation with the given system prompt (may be empty).
    pub fn new(system: impl Into<String>) -> Self {
        Self { system: system.into(), history: Vec::new() }
    }

    /// Append a user turn.
    pub fn user(&mut self, text: impl Into<String>) -> &mut Self {
        self.history.push(Message::user(text));
        self
    }

    /// Append an assistant turn (usually the text streamed from a prior
    /// `generate`, so the model has continuity).
    pub fn assistant(&mut self, text: impl Into<String>) -> &mut Self {
        self.history.push(Message::assistant(text));
        self
    }

    /// Push an arbitrary message.
    pub fn push(&mut self, msg: Message) -> &mut Self {
        self.history.push(msg);
        self
    }

    pub fn system(&self) -> &str {
        &self.system
    }

    pub fn history(&self) -> &[Message] {
        &self.history
    }

    /// Drop oldest non-system turns until the rendered prompt is within
    /// `max_tokens` (estimated). Always keeps the most recent turns so the
    /// model still has immediate context. No-op when the prompt already fits.
    pub fn trim_to_budget(&mut self, max_tokens: usize) {
        if max_tokens == 0 {
            return;
        }
        // Cheap: only re-render while over budget.
        while self.estimate_tokens() > max_tokens && self.history.len() > 1 {
            self.history.remove(0);
        }
    }

    /// Estimated token count of [`Self::render`].
    pub fn estimate_tokens(&self) -> usize {
        (self.render().len() + CHARS_PER_TOKEN - 1) / CHARS_PER_TOKEN
    }

    /// Render the full prompt (system + history) in a stable chat format.
    ///
    /// The format is deliberately simple and model-agnostic:
    /// ```text
    /// <<system>>...
    /// <<user>>...
    /// <<assistant>>...
    /// ```
    /// Most GGUF chat templates accept this; a backend that needs a different
    /// shape (e.g. a specific `chat_template`) can re-serialize `history`
    /// itself — the data is all exposed.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if !self.system.is_empty() {
            out.push_str("<<system>>");
            out.push_str(&self.system);
            out.push('\n');
        }
        for m in &self.history {
            out.push_str("<<");
            out.push_str(m.role.tag());
            out.push_str(">>");
            out.push_str(&m.text);
            out.push('\n');
        }
        out
    }
}

impl Default for Conversation {
    fn default() -> Self {
        Self::new(String::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_all_turns() {
        let mut c = Conversation::new("be brief");
        c.user("hi").assistant("hello");
        let r = c.render();
        assert!(r.contains("<<system>>be brief"));
        assert!(r.contains("<<user>>hi"));
        assert!(r.contains("<<assistant>>hello"));
    }

    #[test]
    fn trims_oldest_keeps_recent() {
        let mut c = Conversation::new("s");
        for i in 0..50 {
            c.user(format!("long turn number {i} padding padding padding padding"));
        }
        let before = c.history().len();
        c.trim_to_budget(64); // ~192 chars
        assert!(c.history().len() < before);
        // The last turn survives.
        assert!(c
            .history()
            .last()
            .unwrap()
            .text
            .contains("long turn number 49"));
    }

    #[test]
    fn budget_zero_is_noop() {
        let mut c = Conversation::new("s");
        c.user("a").user("b");
        c.trim_to_budget(0);
        assert_eq!(c.history().len(), 2);
    }
}
