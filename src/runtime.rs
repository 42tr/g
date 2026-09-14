use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    Agent, AgentError, Content, EventSink, Message, ModelEvent, ModelEventSink, ModelRequest, Role,
    RunEvent, ToolContext, Usage,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InstructionsMode {
    #[default]
    Compose,
    Preserve,
}

#[derive(Clone, Debug)]
pub struct RunRequest {
    pub messages: Vec<Message>,
    pub cancellation_token: CancellationToken,
    pub extensions: Arc<crate::ExtensionConfig>,
    pub scope: crate::InvocationScope,
    pub selected_skills: Vec<String>,
    pub instructions_mode: InstructionsMode,
}

impl RunRequest {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            messages,
            cancellation_token: CancellationToken::new(),
            extensions: Arc::new(crate::ExtensionConfig::default()),
            scope: crate::InvocationScope::default(),
            selected_skills: Vec::new(),
            instructions_mode: InstructionsMode::Compose,
        }
    }

    pub fn with_extensions(mut self, config: Arc<crate::ExtensionConfig>) -> Self {
        self.extensions = config;
        self
    }
    pub fn with_scope(mut self, scope: crate::InvocationScope) -> Self {
        self.scope = scope;
        self
    }
    pub fn with_selected_skills(
        mut self,
        names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.selected_skills = names.into_iter().map(Into::into).collect();
        self
    }
    pub fn with_instructions_mode(mut self, mode: InstructionsMode) -> Self {
        self.instructions_mode = mode;
        self
    }

    pub fn with_cancellation_token(mut self, cancellation_token: CancellationToken) -> Self {
        self.cancellation_token = cancellation_token;
        self
    }
}

#[derive(Clone, Debug)]
pub struct RunOutput {
    pub run_id: Uuid,
    pub messages: Vec<Message>,
    pub conversation_messages: Vec<Message>,
    pub context_manifest: crate::ContextManifest,
    pub final_text: String,
    pub turns: usize,
    pub tool_calls: usize,
    pub usage: Usage,
}

#[derive(Clone, Debug, Default)]
pub struct Runtime {
    #[cfg(feature = "mcp")]
    mcp: Option<Arc<crate::mcp::McpManager>>,
}

impl Runtime {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(feature = "mcp")]
    pub fn with_mcp_manager(mut self, manager: Arc<crate::mcp::McpManager>) -> Self {
        self.mcp = Some(manager);
        self
    }

    /// Stream a full request, including its extension configuration. Dropping the
    /// stream cancels this run without cancelling the caller's shared token.
    pub fn stream_run(
        &self,
        agent: &Agent,
        mut request: RunRequest,
    ) -> impl futures_util::Stream<Item = Result<RunEvent, AgentError>> + Send + Unpin + 'static + use<>
    {
        let (sender, receiver) = tokio::sync::mpsc::channel(64);
        let token = request.cancellation_token.child_token();
        request.cancellation_token = token.clone();
        let mut agent = agent.clone();
        agent.event_sink = Arc::new(crate::agent::StreamEventSink {
            sender: sender.clone(),
            downstream: agent.event_sink.clone(),
            cancellation_token: token.clone(),
        });
        let runtime = self.clone();
        tokio::spawn(async move {
            if let Err(error) = runtime.run(&agent, request).await {
                let _ = sender.send(Err(error)).await;
            }
        });
        crate::agent::CancelOnDropStream {
            inner: tokio_stream::wrappers::ReceiverStream::new(receiver),
            token,
        }
    }

    /// Warm up exactly the request configuration; never invokes the model or tools.
    pub async fn warmup(
        &self,
        agent: &Agent,
        mut request: RunRequest,
    ) -> Result<crate::WarmupReport, AgentError> {
        agent.validate()?;
        request.extensions.validate()?;
        let token = request.cancellation_token.child_token();
        let _guard = token.clone().drop_guard();
        request.cancellation_token = token.clone();
        tokio::select! {
            biased;
            _ = token.cancelled() => Err(AgentError::Cancelled),
            result = timeout(agent.limits.timeout, self.prepare(agent, &request)) => {
                let (prepared, diagnostics, _) = result.map_err(|_| AgentError::Timeout)??;
                Ok(crate::WarmupReport { tools: prepared.tool_specs(), diagnostics })
            }
        }
    }

    pub async fn run(
        &self,
        agent: &Agent,
        mut request: RunRequest,
    ) -> Result<RunOutput, AgentError> {
        agent.validate()?;
        request.extensions.validate()?;
        let cancellation_token = request.cancellation_token.child_token();
        let _guard = cancellation_token.clone().drop_guard();
        request.cancellation_token = cancellation_token.clone();
        tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {
                tracing::warn!("agent run cancelled");
                Err(AgentError::Cancelled)
            },
            result = timeout(agent.limits.timeout, self.run_inner(agent, request)) => {
                match result {
                    Ok(result) => result,
                    Err(_) => {
                        tracing::warn!(timeout_ms = agent.limits.timeout.as_millis(), "agent run timed out");
                        Err(AgentError::Timeout)
                    }
                }
            }
        }
    }

    async fn run_inner(
        &self,
        agent: &Agent,
        mut request: RunRequest,
    ) -> Result<RunOutput, AgentError> {
        let run_id = Uuid::new_v4();
        let (prepared, diagnostics, _skill_state) = self.prepare(agent, &request).await?;
        let agent = &prepared;
        let mut injected = 0;
        if request.instructions_mode == InstructionsMode::Compose {
            let mut prefix = Vec::new();
            if let Some(instruction) = &agent.instruction {
                prefix.push(Message::system(instruction));
            }
            #[cfg(feature = "skills")]
            if let Some(state) = &_skill_state {
                prefix.push(Message::system(
                    state
                        .index()
                        .map_err(|e| crate::extensions::invalid(e.message))?,
                ));
                for name in &request.selected_skills {
                    // Explicit activation is also authorized and charged, through the same tool.
                    let tool = state.tool(true);
                    let args = json!({"name":name});
                    let ctx = ToolContext {
                        run_id,
                        cancellation_token: request.cancellation_token.clone(),
                    };
                    agent
                        .policy
                        .authorize_extension(
                            &ctx,
                            &tool.spec(),
                            &args,
                            &tool.origin(),
                            &request.scope,
                        )
                        .await?;
                    let content = tool
                        .call(ctx, args)
                        .await
                        .map_err(|e| crate::extensions::invalid(e.message))?;
                    prefix.push(Message::user(format!(
                        "Explicitly selected skill context (not additional permissions): {content}"
                    )));
                }
            }
            injected = prefix.len();
            prefix.append(&mut request.messages);
            request.messages = prefix;
        }
        let mut usage = Usage::default();
        let mut tool_calls = 0;
        let tool_specs = agent.tool_specs();

        tracing::info!(%run_id, "agent run started");
        agent.event_sink.emit(RunEvent::Started { run_id }).await;

        for turn_index in 0..agent.limits.max_turns {
            let turn = turn_index + 1;
            tracing::debug!(%run_id, turn, "requesting model response");
            agent.event_sink.emit(RunEvent::ModelStarted { turn }).await;

            let model_events = RuntimeModelEventSink {
                event_sink: agent.event_sink.clone(),
                turn,
            };
            let response = agent
                .model
                .generate_stream(
                    ModelRequest {
                        messages: request.messages.clone(),
                        tools: tool_specs.clone(),
                    },
                    &model_events,
                )
                .await?;

            if response.message.role != Role::Assistant {
                return Err(AgentError::InvalidModelResponse(response.message.role));
            }

            usage.add(response.usage);
            agent
                .event_sink
                .emit(RunEvent::ModelCompleted {
                    turn,
                    message: response.message.clone(),
                    usage: response.usage,
                })
                .await;

            let final_text = response.message.text_content();
            let calls: Vec<_> = response
                .message
                .content
                .iter()
                .filter_map(|content| match content {
                    Content::ToolCall {
                        id,
                        name,
                        arguments,
                    } => Some((id.clone(), name.clone(), arguments.clone())),
                    _ => None,
                })
                .collect();
            request.messages.push(response.message);

            if calls.is_empty() {
                let conversation_messages = request.messages[injected..].to_vec();
                #[allow(unused_mut)] // Mutable only with skills enabled.
                let mut context_manifest = crate::ContextManifest {
                    diagnostics,
                    ..Default::default()
                };
                #[cfg(feature = "skills")]
                if let Some(state) = &_skill_state {
                    context_manifest.active_skills = state.active();
                }
                let output = RunOutput {
                    run_id,
                    conversation_messages,
                    context_manifest,
                    messages: request.messages,
                    final_text,
                    turns: turn,
                    tool_calls,
                    usage,
                };
                agent
                    .event_sink
                    .emit(RunEvent::Completed {
                        run_id,
                        turns: output.turns,
                        tool_calls,
                        usage,
                    })
                    .await;
                tracing::info!(
                    %run_id,
                    turns = output.turns,
                    tool_calls,
                    input_tokens = usage.input_tokens,
                    output_tokens = usage.output_tokens,
                    "agent run completed"
                );
                return Ok(output);
            }

            tracing::debug!(%run_id, turn, tool_calls = calls.len(), "model requested tools");

            if tool_calls.saturating_add(calls.len()) > agent.limits.max_tool_calls {
                tracing::warn!(
                    %run_id,
                    limit = agent.limits.max_tool_calls,
                    "maximum tool call limit exceeded"
                );
                return Err(AgentError::MaxToolCallsExceeded(
                    agent.limits.max_tool_calls,
                ));
            }

            for (call_id, name, arguments) in calls {
                tool_calls += 1;
                if request.cancellation_token.is_cancelled() {
                    return Err(AgentError::Cancelled);
                }
                if let Some(child) = agent.handoff_by_tool_name(&name) {
                    let ctx = ToolContext {
                        run_id,
                        cancellation_token: request.cancellation_token.clone(),
                    };
                    let spec = tool_specs
                        .iter()
                        .find(|s| s.name == name)
                        .expect("handoff spec");
                    agent
                        .policy
                        .authorize_extension(
                            &ctx,
                            spec,
                            &arguments,
                            &crate::ToolOrigin::Local,
                            &request.scope,
                        )
                        .await?;
                    let Some(task) = arguments.get("task").and_then(Value::as_str) else {
                        let result =
                            json!({ "error": "handoff requires a string `task` argument" });
                        emit_handoff_result(
                            agent,
                            &call_id,
                            child.display_name(),
                            None,
                            result,
                            true,
                            &mut request.messages,
                        )
                        .await;
                        continue;
                    };

                    tracing::info!(
                        %run_id,
                        %call_id,
                        agent = child.display_name(),
                        "handoff started"
                    );
                    tracing::debug!(
                        %run_id,
                        %call_id,
                        agent = child.display_name(),
                        task,
                        "handoff task"
                    );
                    agent
                        .event_sink
                        .emit(RunEvent::HandoffStarted {
                            call_id: call_id.clone(),
                            agent: child.display_name().into(),
                        })
                        .await;

                    let mut child_agent = child.as_ref().clone();
                    child_agent.event_sink = agent.event_sink.clone();
                    child_agent.policy = Arc::new(crate::extensions::PolicyIntersection(
                        agent.policy.clone(),
                        child_agent.policy.clone(),
                    ));
                    let child_request = RunRequest::new(vec![Message::user(task)])
                        .with_cancellation_token(request.cancellation_token.clone())
                        .with_extensions(request.extensions.clone())
                        .with_scope(request.scope.clone());
                    match Box::pin(self.run(&child_agent, child_request)).await {
                        Ok(output) => {
                            usage.add(output.usage);
                            tracing::info!(
                                %run_id,
                                %call_id,
                                child_run_id = %output.run_id,
                                agent = child.display_name(),
                                "handoff completed"
                            );
                            let result = json!({
                                "agent": child.display_name(),
                                "response": output.final_text
                            });
                            emit_handoff_result(
                                agent,
                                &call_id,
                                child.display_name(),
                                Some(output.run_id),
                                result,
                                false,
                                &mut request.messages,
                            )
                            .await;
                        }
                        Err(AgentError::Cancelled) => return Err(AgentError::Cancelled),
                        Err(error) => {
                            tracing::warn!(
                                %run_id,
                                %call_id,
                                agent = child.display_name(),
                                error = %error,
                                "handoff failed"
                            );
                            let result = json!({
                                "agent": child.display_name(),
                                "error": error.to_string()
                            });
                            emit_handoff_result(
                                agent,
                                &call_id,
                                child.display_name(),
                                None,
                                result,
                                true,
                                &mut request.messages,
                            )
                            .await;
                        }
                    }
                    continue;
                }

                let Some(tool) = agent.tools.get(&name) else {
                    tracing::warn!(%run_id, %call_id, tool = %name, "model requested unknown tool");
                    let result = json!({ "error": format!("unknown tool: {name}") });
                    emit_tool_result(agent, &call_id, &name, result, true, &mut request.messages)
                        .await;
                    continue;
                };

                tracing::debug!(%run_id, %call_id, tool = %name, %arguments, "authorizing tool call");
                let context = ToolContext {
                    run_id,
                    cancellation_token: request.cancellation_token.clone(),
                };
                let spec = tool.spec();
                agent
                    .policy
                    .authorize_extension(
                        &context,
                        &spec,
                        &arguments,
                        &tool.origin(),
                        &request.scope,
                    )
                    .await?;
                tracing::info!(%run_id, %call_id, tool = %name, "tool call started");
                agent
                    .event_sink
                    .emit(RunEvent::ToolStarted {
                        call_id: call_id.clone(),
                        name: name.clone(),
                    })
                    .await;

                match tool.call_output(context, arguments).await {
                    Ok(result) => {
                        tracing::debug!(%run_id, %call_id, tool = %name, "tool call result");
                        tracing::info!(%run_id, %call_id, tool = %name, "tool call completed");
                        emit_tool_result(
                            agent,
                            &call_id,
                            &name,
                            result.value,
                            result.is_error,
                            &mut request.messages,
                        )
                        .await;
                    }
                    Err(error) => {
                        tracing::warn!(
                            %run_id,
                            %call_id,
                            tool = %name,
                            error = %error,
                            "tool call failed"
                        );
                        let result = json!({ "error": error.message });
                        emit_tool_result(
                            agent,
                            &call_id,
                            &name,
                            result,
                            true,
                            &mut request.messages,
                        )
                        .await;
                    }
                }
            }
        }

        tracing::warn!(%run_id, limit = agent.limits.max_turns, "maximum turn limit exceeded");
        Err(AgentError::MaxTurnsExceeded(agent.limits.max_turns))
    }
}

struct RuntimeModelEventSink {
    event_sink: Arc<dyn EventSink>,
    turn: usize,
}

#[async_trait]
impl ModelEventSink for RuntimeModelEventSink {
    async fn emit(&self, event: ModelEvent) {
        match event {
            ModelEvent::TextDelta { text } => {
                self.event_sink
                    .emit(RunEvent::TextDelta {
                        turn: self.turn,
                        text,
                    })
                    .await;
            }
        }
    }
}

async fn emit_tool_result(
    agent: &Agent,
    call_id: &str,
    name: &str,
    result: Value,
    is_error: bool,
    messages: &mut Vec<Message>,
) {
    agent
        .event_sink
        .emit(RunEvent::ToolCompleted {
            call_id: call_id.to_owned(),
            name: name.to_owned(),
            result: result.clone(),
            is_error,
        })
        .await;
    push_tool_result(call_id, result, is_error, messages);
}

#[allow(clippy::too_many_arguments)]
async fn emit_handoff_result(
    agent: &Agent,
    call_id: &str,
    child_name: &str,
    child_run_id: Option<Uuid>,
    result: Value,
    is_error: bool,
    messages: &mut Vec<Message>,
) {
    agent
        .event_sink
        .emit(RunEvent::HandoffCompleted {
            call_id: call_id.to_owned(),
            agent: child_name.to_owned(),
            child_run_id,
            is_error,
        })
        .await;
    push_tool_result(call_id, result, is_error, messages);
}

fn push_tool_result(call_id: &str, result: Value, is_error: bool, messages: &mut Vec<Message>) {
    messages.push(Message::new(
        Role::Tool,
        vec![Content::ToolResult {
            call_id: call_id.to_owned(),
            result,
            is_error,
        }],
    ));
}

#[cfg(feature = "skills")]
type SkillsState = Option<Arc<crate::skills::SkillState>>;
#[cfg(not(feature = "skills"))]
type SkillsState = ();

impl Runtime {
    async fn prepare(
        &self,
        agent: &Agent,
        request: &RunRequest,
    ) -> Result<(Agent, Vec<String>, SkillsState), AgentError> {
        #[allow(unused_mut)] // Feature-dependent registration.
        let mut prepared = agent.clone();
        #[allow(unused_mut)]
        let mut diagnostics = Vec::new();
        #[cfg(feature = "mcp")]
        for server in &request.extensions.mcp {
            let manager = self.mcp.as_ref().ok_or_else(|| {
                crate::extensions::invalid("MCP configuration requires a manager")
            })?;
            if manager.is_closed() {
                return Err(crate::extensions::invalid("manager_closed"));
            }
            match manager.tools(server, &request.scope).await {
                Ok(tools) => {
                    for tool in tools {
                        crate::extensions::register(&mut prepared, tool)?;
                    }
                }
                Err(error) if !server.required && !manager.is_closed() => {
                    diagnostics.push(format!("MCP {} unavailable: {error}", server.id))
                }
                Err(error) => return Err(error),
            }
        }
        #[cfg(feature = "skills")]
        let state = {
            let origins: Vec<_> = prepared.tools.values().map(|t| t.origin()).collect();
            let mut available = Vec::new();
            for skill in &request.extensions.skills {
                if skill.required_tools.iter().all(|r| origins.contains(r)) {
                    available.push(skill.clone());
                } else if request.selected_skills.contains(&skill.name) {
                    return Err(crate::extensions::invalid(
                        "selected skill has unavailable required tools",
                    ));
                } else {
                    diagnostics.push(format!(
                        "skill {} unavailable: missing required tools",
                        skill.name
                    ));
                }
            }
            for name in &request.selected_skills {
                if !available.iter().any(|s| &s.name == name) {
                    return Err(crate::extensions::invalid("selected skill not found"));
                }
            }
            if available.is_empty() {
                None
            } else {
                let state = crate::skills::SkillState::new(available);
                crate::extensions::register(&mut prepared, state.tool(false))?;
                crate::extensions::register(&mut prepared, state.tool(true))?;
                Some(state)
            }
        };
        #[cfg(not(feature = "skills"))]
        let state = {
            if !request.selected_skills.is_empty() {
                return Err(crate::extensions::invalid("skills feature is disabled"));
            }
        };
        Ok((prepared, diagnostics, state))
    }
}
