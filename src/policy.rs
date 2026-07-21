use async_trait::async_trait;
use serde_json::Value;

use crate::{PolicyError, ToolContext, ToolSpec};

#[async_trait]
pub trait Policy: Send + Sync {
    async fn authorize(
        &self,
        context: &ToolContext,
        tool: &ToolSpec,
        arguments: &Value,
    ) -> Result<(), PolicyError>;
}

#[derive(Debug, Default)]
pub struct AllowAll;

#[async_trait]
impl Policy for AllowAll {
    async fn authorize(
        &self,
        _context: &ToolContext,
        _tool: &ToolSpec,
        _arguments: &Value,
    ) -> Result<(), PolicyError> {
        Ok(())
    }
}
