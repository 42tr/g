//! Host-owned tasks can outlive a model turn or a run. The host is responsible
//! for persistence, independent cancellation, resource limits and recovery.
use crate::{Tool, ToolBehavior, ToolError, ToolSpec};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

pub const TASK_INSTRUCTION: &str = "\nTask execution: use run_tools to start independent tools concurrently. Its response may contain pending task IDs. Use wait_tools to receive results (timeouts only end the wait), get_tasks to inspect progress, cancel_tools to stop unnecessary tasks, and detach_tools to let tasks continue after your reply. Never claim a pending task succeeded. Before finishing, receive required results and detach or cancel remaining tasks. command runs CLI programs including codex/claude; use noninteractive commands. Do not repeatedly poll without waiting. Parent task IDs denote actual subtasks, not mere dependencies.\n";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCall {
    pub tool: String,
    #[serde(default = "empty_object")]
    pub arguments: Value,
    pub title: Option<String>,
    pub parent_task_id: Option<String>,
    pub execution_timeout_secs: Option<u64>,
}
fn empty_object() -> Value {
    json!({})
}

/// Authorization has already been checked by the runtime. The backend must
/// use its own cancellation token, never the parent run's token, for execution.
pub struct TaskSubmission {
    pub run_id: Uuid,
    pub call_id: String,
    pub index: usize,
    pub request: TaskCall,
    pub tool: Arc<dyn Tool>,
}

#[async_trait]
pub trait TaskBackend: Send + Sync {
    async fn submit(&self, tasks: Vec<TaskSubmission>) -> Result<Vec<String>, ToolError>;
    async fn control(&self, operation: &str, arguments: Value) -> Result<Value, ToolError>;
    /// Called after the corresponding model tool response has reached the event sink.
    async fn acknowledge(&self, _result: &Value) -> Result<(), ToolError> {
        Ok(())
    }
    /// True only when required results were delivered and no attached task is active.
    async fn ready_to_finish(&self) -> Result<bool, ToolError>;
}

pub fn is_task_control(name: &str) -> bool {
    matches!(
        name,
        "run_tools" | "wait_tools" | "get_tasks" | "detach_tools" | "cancel_tools"
    )
}

pub(crate) fn task_specs() -> Vec<ToolSpec> {
    let ids = json!({"type":"array","items":{"type":"string"},"maxItems":64});
    let wait = json!({"type":"integer","minimum":0,"maximum":604800});
    [
        ("run_tools", "Submit tools as host-owned tasks. Wait for any/all result or return immediately. Each actual call counts against the tool budget.", json!({
            "type":"object", "properties": {
                "calls":{"type":"array","minItems":1,"maxItems":32,"items":{"type":"object","properties":{
                    "tool":{"type":"string"},"arguments":{"type":"object"},"title":{"type":"string","maxLength":200},
                    "parent_task_id":{"type":"string"},"execution_timeout_secs":{"type":"integer","minimum":1}
                },"required":["tool","arguments"],"additionalProperties":false}},
                "yield_when":{"type":"string","enum":["none","any","all"]},"wait_timeout_secs":wait
            },"required":["calls"],"additionalProperties":false})),
        ("wait_tools", "Receive task results; waiting does not cancel tasks. Defaults to any result, 30 seconds.", json!({"type":"object","properties":{
            "task_ids":ids,"yield_when":{"type":"string","enum":["any","all"]},"wait_timeout_secs":wait
        },"required":["task_ids"],"additionalProperties":false})),
        ("get_tasks", "Read task status, recent output and results. Omit IDs to list this conversation's tasks.", json!({"type":"object","properties":{"task_ids":ids},"additionalProperties":false})),
        ("detach_tools", "Allow selected tasks to continue after this reply finishes or is stopped.", json!({"type":"object","properties":{"task_ids":ids},"required":["task_ids"],"additionalProperties":false})),
        ("cancel_tools", "Cancel selected tasks and their descendants. Cancellation does not undo side effects.", json!({"type":"object","properties":{"task_ids":ids,"reason":{"type":"string"}},"required":["task_ids"],"additionalProperties":false})),
    ].into_iter().map(|(name, description, input_schema)| ToolSpec {
        name:name.into(), description:description.into(), input_schema, behavior:ToolBehavior::default(),
    }).collect()
}
