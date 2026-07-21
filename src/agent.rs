use std::{collections::HashMap, sync::Arc, time::Duration};

use crate::{
    AgentError, AllowAll, EventSink, IntoTool, Message, Model, NoopEventSink, Policy, RunOutput,
    RunRequest, Runtime, Tool, ToolSpec,
};

#[derive(Clone, Copy, Debug)]
pub struct RunLimits {
    pub max_turns: usize,
    pub max_tool_calls: usize,
    pub timeout: Duration,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            max_turns: 16,
            max_tool_calls: 32,
            timeout: Duration::from_secs(120),
        }
    }
}

pub struct Agent {
    pub(crate) model: Arc<dyn Model>,
    pub(crate) tools: HashMap<String, Arc<dyn Tool>>,
    pub(crate) policy: Arc<dyn Policy>,
    pub(crate) event_sink: Arc<dyn EventSink>,
    pub(crate) limits: RunLimits,
    instruction: Option<String>,
}

impl Agent {
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self {
            model,
            tools: HashMap::new(),
            policy: Arc::new(AllowAll),
            event_sink: Arc::new(NoopEventSink),
            limits: RunLimits::default(),
            instruction: None,
        }
    }

    pub fn instruction(mut self, instruction: impl Into<String>) -> Self {
        self.instruction = Some(instruction.into());
        self
    }

    pub fn tool(mut self, tool: impl IntoTool) -> Self {
        let tool = tool.into_tool();
        self.tools.insert(tool.spec().name, tool);
        self
    }

    pub fn tools<I>(mut self, tools: I) -> Self
    where
        I: IntoIterator,
        I::Item: IntoTool,
    {
        for tool in tools {
            let tool = tool.into_tool();
            self.tools.insert(tool.spec().name, tool);
        }
        self
    }

    pub fn register_tool(&mut self, tool: Arc<dyn Tool>) -> Result<(), AgentError> {
        let name = tool.spec().name;
        if self.tools.contains_key(&name) {
            return Err(AgentError::DuplicateTool(name));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    pub fn with_policy(mut self, policy: Arc<dyn Policy>) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_event_sink(mut self, event_sink: Arc<dyn EventSink>) -> Self {
        self.event_sink = event_sink;
        self
    }

    pub fn with_limits(mut self, limits: RunLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<_> = self.tools.values().map(|tool| tool.spec()).collect();
        specs.sort_by(|left, right| left.name.cmp(&right.name));
        specs
    }

    pub async fn run(&self, prompt: impl Into<String>) -> Result<RunOutput, AgentError> {
        let mut messages = Vec::with_capacity(2);
        if let Some(instruction) = &self.instruction {
            messages.push(Message::system(instruction));
        }
        messages.push(Message::user(prompt));
        Runtime::new().run(self, RunRequest::new(messages)).await
    }
}
