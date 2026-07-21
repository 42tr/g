use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use crate::{Message, Usage};

#[derive(Clone, Debug)]
pub enum RunEvent {
    Started {
        run_id: Uuid,
    },
    ModelStarted {
        turn: usize,
    },
    ModelCompleted {
        turn: usize,
        message: Message,
        usage: Usage,
    },
    ToolStarted {
        call_id: String,
        name: String,
    },
    ToolCompleted {
        call_id: String,
        name: String,
        result: Value,
        is_error: bool,
    },
    Completed {
        run_id: Uuid,
        turns: usize,
        tool_calls: usize,
        usage: Usage,
    },
}

#[async_trait]
pub trait EventSink: Send + Sync {
    async fn emit(&self, event: RunEvent);
}

#[derive(Debug, Default)]
pub struct NoopEventSink;

#[async_trait]
impl EventSink for NoopEventSink {
    async fn emit(&self, _event: RunEvent) {}
}
