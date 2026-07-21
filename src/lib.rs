extern crate self as g;

mod agent;
mod error;
mod event;
mod message;
mod model;
mod policy;
pub mod providers;
mod runtime;
mod tool;

pub use agent::{Agent, RunLimits};
pub use error::{AgentError, ModelError, PolicyError, ToolError};
pub use event::{EventSink, NoopEventSink, RunEvent};
pub use g_macros::tool;
pub use message::{Content, Message, Role};
pub use model::{Model, ModelRequest, ModelResponse, Usage};
pub use policy::{AllowAll, Policy};
pub use providers::openai::OpenAIModel;
pub use runtime::{RunOutput, RunRequest, Runtime};
pub use tool::{IntoTool, Tool, ToolBehavior, ToolContext, ToolSpec};

pub type ToolCallError = ToolError;

#[doc(hidden)]
pub mod __private {
    pub use async_trait;
    pub use schemars;
    pub use serde_json;
}
