use serde_json::{Value, json};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    Agent, AgentError, Content, Message, ModelRequest, Role, RunEvent, ToolContext, Usage,
};

#[derive(Clone, Debug)]
pub struct RunRequest {
    pub messages: Vec<Message>,
    pub cancellation_token: CancellationToken,
}

impl RunRequest {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            messages,
            cancellation_token: CancellationToken::new(),
        }
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
    pub final_text: String,
    pub turns: usize,
    pub tool_calls: usize,
    pub usage: Usage,
}

#[derive(Debug, Default)]
pub struct Runtime;

impl Runtime {
    pub fn new() -> Self {
        Self
    }

    pub async fn run(&self, agent: &Agent, request: RunRequest) -> Result<RunOutput, AgentError> {
        let cancellation_token = request.cancellation_token.clone();
        tokio::select! {
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
        let mut usage = Usage::default();
        let mut tool_calls = 0;
        let tool_specs = agent.tool_specs();

        tracing::info!(%run_id, "agent run started");
        agent.event_sink.emit(RunEvent::Started { run_id }).await;

        for turn_index in 0..agent.limits.max_turns {
            let turn = turn_index + 1;
            tracing::debug!(%run_id, turn, "requesting model response");
            agent.event_sink.emit(RunEvent::ModelStarted { turn }).await;

            let response = agent
                .model
                .generate(ModelRequest {
                    messages: request.messages.clone(),
                    tools: tool_specs.clone(),
                })
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
                let output = RunOutput {
                    run_id,
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
                agent.policy.authorize(&context, &spec, &arguments).await?;
                tracing::info!(%run_id, %call_id, tool = %name, "tool call started");
                agent
                    .event_sink
                    .emit(RunEvent::ToolStarted {
                        call_id: call_id.clone(),
                        name: name.clone(),
                    })
                    .await;

                match tool.call(context, arguments).await {
                    Ok(result) => {
                        tracing::debug!(%run_id, %call_id, tool = %name, %result, "tool call result");
                        tracing::info!(%run_id, %call_id, tool = %name, "tool call completed");
                        emit_tool_result(
                            agent,
                            &call_id,
                            &name,
                            result,
                            false,
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
    messages.push(Message::new(
        Role::Tool,
        vec![Content::ToolResult {
            call_id: call_id.to_owned(),
            result,
            is_error,
        }],
    ));
}
