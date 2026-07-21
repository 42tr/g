use std::{collections::HashMap, sync::Arc, time::Duration};

use crate::{
    AgentError, AllowAll, EventSink, IntoTool, Message, Model, NoopEventSink, Policy, RunOutput,
    RunRequest, Runtime, Tool, ToolBehavior, ToolSpec,
};
use serde_json::json;

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
    name: Option<String>,
    description: Option<String>,
    handoffs: Vec<Arc<Agent>>,
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
            name: None,
            description: None,
            handoffs: Vec::new(),
        }
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
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

    pub fn handoff<I>(mut self, agents: I) -> Self
    where
        I: IntoIterator<Item = Agent>,
    {
        self.handoffs.extend(agents.into_iter().map(Arc::new));
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
        specs.extend(self.handoffs.iter().filter_map(|agent| {
            let name = agent.name.as_deref()?;
            Some(ToolSpec {
                name: handoff_tool_name(name),
                description: agent
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("Hand off the task to the {name} agent")),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "description": "The task and relevant context for the agent"
                        }
                    },
                    "required": ["task"],
                    "additionalProperties": false
                }),
                behavior: ToolBehavior::default(),
            })
        }));
        specs.sort_by(|left, right| left.name.cmp(&right.name));
        specs
    }

    pub async fn run(&self, prompt: impl Into<String>) -> Result<RunOutput, AgentError> {
        Runtime::new()
            .run(self, RunRequest::new(self.prompt_messages(prompt.into())))
            .await
    }

    pub(crate) fn prompt_messages(&self, prompt: String) -> Vec<Message> {
        let mut messages = Vec::with_capacity(2);
        if let Some(instruction) = &self.instruction {
            messages.push(Message::system(instruction));
        }
        messages.push(Message::user(prompt));
        messages
    }

    pub(crate) fn handoff_by_tool_name(&self, tool_name: &str) -> Option<&Arc<Agent>> {
        self.handoffs.iter().find(|agent| {
            agent
                .name
                .as_deref()
                .is_some_and(|name| handoff_tool_name(name) == tool_name)
        })
    }

    pub(crate) fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or("agent")
    }

    pub(crate) fn validate(&self) -> Result<(), AgentError> {
        let mut names = std::collections::HashSet::new();
        for agent in &self.handoffs {
            let name = agent.name.as_deref().ok_or_else(|| {
                AgentError::InvalidConfiguration("handoff agents must have a name".into())
            })?;
            if name.trim().is_empty() {
                return Err(AgentError::InvalidConfiguration(
                    "handoff agent name cannot be empty".into(),
                ));
            }
            let tool_name = handoff_tool_name(name);
            if !names.insert(tool_name.clone()) {
                return Err(AgentError::InvalidConfiguration(format!(
                    "duplicate handoff name `{name}`"
                )));
            }
            if self.tools.contains_key(&tool_name) {
                return Err(AgentError::InvalidConfiguration(format!(
                    "handoff tool `{tool_name}` conflicts with a registered tool"
                )));
            }
            agent.validate()?;
        }
        Ok(())
    }
}

fn handoff_tool_name(name: &str) -> String {
    let name: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("handoff_to_{name}")
}
