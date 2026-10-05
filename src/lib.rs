extern crate self as g;

mod agent;
mod error;
mod event;
mod extensions;
#[cfg(feature = "mcp")]
pub mod mcp;
mod message;
mod model;
mod policy;
pub mod providers;
mod runtime;
#[cfg(feature = "skills")]
pub mod skills;
mod task;
mod tool;
pub use task::{TASK_INSTRUCTION, TaskBackend, TaskCall, TaskSubmission};

pub use agent::{Agent, PolicyDenial, RetryPolicy, RunLimits};
pub use error::{AgentError, ModelError, PolicyError, ToolError};
pub use event::{EventSink, NoopEventSink, RunEvent};
pub use extensions::{ContextManifest, ExtensionConfig, InvocationScope, ToolOrigin, WarmupReport};
pub use g_macros::tool;
pub use message::{Content, ImageDetail, ImageSource, IntoPrompt, Message, Role};
pub use model::{Model, ModelEvent, ModelEventSink, ModelRequest, ModelResponse, Usage};
pub use policy::{AllowAll, Policy};
pub use providers::openai::OpenAIModel;
pub use providers::openai_chat::OpenAIChatModel;
pub use providers::openai_from_env;
pub use runtime::{InstructionsMode, RunOutput, RunRequest, Runtime};
pub use tool::{IntoTool, Tool, ToolBehavior, ToolContext, ToolOutput, ToolSpec};

pub type ToolCallError = ToolError;

#[doc(hidden)]
pub mod __private {
    pub use async_trait;
    pub use schemars;
    pub use serde_json;
}
