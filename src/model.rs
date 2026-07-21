use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{Message, ModelError, ToolSpec};

#[derive(Clone, Debug)]
pub struct ModelRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
    }
}

#[derive(Clone, Debug)]
pub struct ModelResponse {
    pub message: Message,
    pub usage: Usage,
}

impl ModelResponse {
    pub fn new(message: Message) -> Self {
        Self {
            message,
            usage: Usage::default(),
        }
    }
}

#[async_trait]
pub trait Model: Send + Sync {
    async fn generate(&self, request: ModelRequest) -> Result<ModelResponse, ModelError>;
}
