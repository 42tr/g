use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::ToolError;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolBehavior {
    pub read_only: bool,
    pub idempotent: bool,
    pub parallel_safe: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub behavior: ToolBehavior,
}

#[derive(Clone, Debug)]
pub struct ToolContext {
    pub run_id: Uuid,
    pub cancellation_token: CancellationToken,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    async fn call(&self, context: ToolContext, input: Value) -> Result<Value, ToolError>;
}

pub trait IntoTool {
    fn into_tool(self) -> Arc<dyn Tool>;
}

impl<T> IntoTool for T
where
    T: Tool + 'static,
{
    fn into_tool(self) -> Arc<dyn Tool> {
        Arc::new(self)
    }
}

impl IntoTool for Arc<dyn Tool> {
    fn into_tool(self) -> Arc<dyn Tool> {
        self
    }
}
