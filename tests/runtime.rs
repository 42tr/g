use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use g::{
    Agent, AgentError, Content, Message, Model, ModelError, ModelRequest, ModelResponse, Role,
    RunLimits, RunRequest, Runtime, Tool, ToolBehavior, ToolContext, ToolError, ToolSpec,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct ScriptedModel {
    responses: Mutex<VecDeque<Result<ModelResponse, ModelError>>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptedModel {
    fn new(responses: impl IntoIterator<Item = ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().map(Ok).collect()),
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn generate(&self, request: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.requests.lock().unwrap().push(request);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted model ran out of responses")
    }
}

struct AddTool;

#[async_trait]
impl Tool for AddTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "add".into(),
            description: "Add two integers".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "left": { "type": "integer" },
                    "right": { "type": "integer" }
                },
                "required": ["left", "right"]
            }),
            behavior: ToolBehavior {
                read_only: true,
                idempotent: true,
                parallel_safe: true,
            },
        }
    }

    async fn call(&self, _context: ToolContext, input: Value) -> Result<Value, ToolError> {
        let left = input["left"]
            .as_i64()
            .ok_or_else(|| ToolError::new("left must be an integer"))?;
        let right = input["right"]
            .as_i64()
            .ok_or_else(|| ToolError::new("right must be an integer"))?;
        Ok(json!({ "sum": left + right }))
    }
}

fn tool_call_response() -> ModelResponse {
    ModelResponse::new(Message::new(
        Role::Assistant,
        vec![Content::ToolCall {
            id: "call-1".into(),
            name: "add".into(),
            arguments: json!({ "left": 20, "right": 22 }),
        }],
    ))
}

#[tokio::test]
async fn returns_a_direct_model_answer() {
    let model = Arc::new(ScriptedModel::new([ModelResponse::new(
        Message::assistant("hello"),
    )]));
    let agent = Agent::new(model.clone()).instruction("Answer briefly.");

    let output = agent.run("hi").await.unwrap();

    assert_eq!(output.final_text, "hello");
    assert_eq!(output.turns, 1);
    assert_eq!(output.tool_calls, 0);
    assert_eq!(output.messages.len(), 3);
    assert_eq!(
        model.requests.lock().unwrap()[0].messages[0].role,
        Role::System
    );
}

#[tokio::test]
async fn executes_a_tool_and_sends_its_result_to_the_model() {
    let model = Arc::new(ScriptedModel::new([
        tool_call_response(),
        ModelResponse::new(Message::assistant("The answer is 42.")),
    ]));
    let agent = Agent::new(model.clone()).tools([AddTool]);

    let output = Runtime::new()
        .run(
            &agent,
            RunRequest::new(vec![Message::user("What is 20 + 22?")]),
        )
        .await
        .unwrap();

    assert_eq!(output.final_text, "The answer is 42.");
    assert_eq!(output.turns, 2);
    assert_eq!(output.tool_calls, 1);

    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let tool_message = requests[1].messages.last().unwrap();
    assert_eq!(tool_message.role, Role::Tool);
    assert_eq!(
        tool_message.content,
        vec![Content::ToolResult {
            call_id: "call-1".into(),
            result: json!({ "sum": 42 }),
            is_error: false,
        }]
    );
}

#[tokio::test]
async fn stops_when_the_turn_limit_is_reached() {
    let model = Arc::new(ScriptedModel::new([tool_call_response()]));
    let mut agent = Agent::new(model).with_limits(RunLimits {
        max_turns: 1,
        max_tool_calls: 10,
        timeout: Duration::from_secs(1),
    });
    agent.register_tool(Arc::new(AddTool)).unwrap();

    let error = Runtime::new()
        .run(&agent, RunRequest::new(vec![Message::user("keep going")]))
        .await
        .unwrap_err();

    assert!(matches!(error, AgentError::MaxTurnsExceeded(1)));
}

struct PendingModel;

#[async_trait]
impl Model for PendingModel {
    async fn generate(&self, _request: ModelRequest) -> Result<ModelResponse, ModelError> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn supports_cancellation() {
    let agent = Agent::new(Arc::new(PendingModel));
    let cancellation_token = CancellationToken::new();
    cancellation_token.cancel();

    let error = Runtime::new()
        .run(
            &agent,
            RunRequest::new(vec![Message::user("wait")])
                .with_cancellation_token(cancellation_token),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, AgentError::Cancelled));
}

#[tokio::test]
async fn hands_a_task_to_a_named_agent_and_returns_the_result_to_the_parent() {
    let child_model = Arc::new(ScriptedModel::new([ModelResponse::new(
        Message::assistant("42"),
    )]));
    let child = Agent::new(child_model)
        .name("math")
        .description("Solve arithmetic tasks")
        .instruction("Return only the answer.");

    let parent_model = Arc::new(ScriptedModel::new([
        ModelResponse::new(Message::new(
            Role::Assistant,
            vec![Content::ToolCall {
                id: "handoff-1".into(),
                name: "handoff_to_math".into(),
                arguments: json!({ "task": "Calculate 20 + 22" }),
            }],
        )),
        ModelResponse::new(Message::assistant("The math agent returned 42.")),
    ]));
    let parent = Agent::new(parent_model.clone()).handoff([child]);

    let output = parent.run("What is 20 + 22?").await.unwrap();

    assert_eq!(output.final_text, "The math agent returned 42.");
    assert_eq!(output.tool_calls, 1);
    let requests = parent_model.requests.lock().unwrap();
    assert_eq!(requests[0].tools[0].name, "handoff_to_math");
    assert_eq!(
        requests[0].tools[0].input_schema["required"],
        json!(["task"])
    );
    assert_eq!(
        requests[1].messages.last().unwrap().content,
        vec![Content::ToolResult {
            call_id: "handoff-1".into(),
            result: json!({ "agent": "math", "response": "42" }),
            is_error: false,
        }]
    );
}

#[tokio::test]
async fn rejects_an_unnamed_handoff_agent_before_calling_the_model() {
    let child = Agent::new(Arc::new(ScriptedModel::new(Vec::<ModelResponse>::new())));
    let parent_model = Arc::new(ScriptedModel::new(Vec::<ModelResponse>::new()));
    let parent = Agent::new(parent_model.clone()).handoff([child]);

    let error = parent.run("route this").await.unwrap_err();

    assert!(matches!(error, AgentError::InvalidConfiguration(_)));
    assert!(parent_model.requests.lock().unwrap().is_empty());
}
