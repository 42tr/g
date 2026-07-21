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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelEvent {
    TextDelta { text: String },
}

#[async_trait]
pub trait ModelEventSink: Send + Sync {
    async fn emit(&self, event: ModelEvent);
}

#[async_trait]
pub trait Model: Send + Sync {
    async fn generate(&self, request: ModelRequest) -> Result<ModelResponse, ModelError>;

    async fn generate_stream(
        &self,
        request: ModelRequest,
        event_sink: &dyn ModelEventSink,
    ) -> Result<ModelResponse, ModelError> {
        let response = self.generate(request).await?;
        let text = response.message.text_content();
        if !text.is_empty() {
            event_sink.emit(ModelEvent::TextDelta { text }).await;
        }
        Ok(response)
    }
}
