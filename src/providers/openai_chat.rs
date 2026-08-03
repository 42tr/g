use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Content, ImageDetail, ImageSource, Message, Model, ModelError, ModelEvent, ModelEventSink,
    ModelRequest, ModelResponse, Role, ToolSpec, Usage,
};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "gpt-4o";
const PROVIDER_NAME: &str = "openai_chat";

#[derive(Clone, Debug)]
pub struct OpenAIChatModel {
    client: Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl OpenAIChatModel {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
            model: model.into(),
            base_url: DEFAULT_BASE_URL.into(),
        }
    }

    pub fn from_env() -> Result<Self, ModelError> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| ModelError::new("OPENAI_API_KEY is not set"))?;
        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
        let mut instance = Self::new(api_key, model);
        if let Ok(base_url) = std::env::var("OPENAI_BASE_URL") {
            instance.base_url = base_url.trim_end_matches('/').to_owned();
        }
        Ok(instance)
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_owned();
        self
    }
}

#[async_trait]
impl Model for OpenAIChatModel {
    async fn generate(&self, request: ModelRequest) -> Result<ModelResponse, ModelError> {
        tracing::debug!(
            model = %self.model,
            messages = request.messages.len(),
            tools = request.tools.len(),
            "sending OpenAI Chat Completions API request"
        );
        let body = self.request_body(&request, false)?;

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|error| ModelError::retryable(error.to_string()))?;
        let status = response.status();
        tracing::debug!(%status, model = %self.model, "received OpenAI Chat Completions API response");
        let body = response
            .text()
            .await
            .map_err(|error| ModelError::retryable(error.to_string()))?;

        if !status.is_success() {
            tracing::warn!(%status, model = %self.model, "OpenAI Chat Completions API request failed");
            return Err(api_status_error(status, &body));
        }

        let response: ChatCompletionResponse = serde_json::from_str(&body)
            .map_err(|error| ModelError::new(format!("invalid OpenAI response: {error}")))?;
        let response = response_to_model(response)?;
        tracing::debug!(
            model = %self.model,
            input_tokens = response.usage.input_tokens,
            output_tokens = response.usage.output_tokens,
            "parsed OpenAI Chat Completions response"
        );
        Ok(response)
    }

    async fn generate_stream(
        &self,
        request: ModelRequest,
        event_sink: &dyn ModelEventSink,
    ) -> Result<ModelResponse, ModelError> {
        tracing::debug!(
            model = %self.model,
            messages = request.messages.len(),
            tools = request.tools.len(),
            "opening OpenAI Chat Completions API stream"
        );
        let body = self.request_body(&request, true)?;
        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|error| ModelError::retryable(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .map_err(|error| ModelError::retryable(error.to_string()))?;
            return Err(api_status_error(status, &body));
        }

        let mut events = response.bytes_stream().eventsource();
        let mut accumulator = StreamAccumulator::default();
        while let Some(event) = events.next().await {
            let event = event
                .map_err(|error| ModelError::retryable(format!("OpenAI stream error: {error}")))?;
            if event.data == "[DONE]" {
                continue;
            }
            let chunk: StreamChunk = serde_json::from_str(&event.data).map_err(|error| {
                ModelError::new(format!("invalid OpenAI stream event: {error}"))
            })?;
            if let Some(choice) = chunk.choices.into_iter().next() {
                accumulator.apply_delta(choice.delta, event_sink).await?;
            }
            if let Some(usage) = chunk.usage {
                accumulator.usage = Some(Usage {
                    input_tokens: usage.prompt_tokens,
                    output_tokens: usage.completion_tokens,
                });
            }
        }

        accumulator.into_response()
    }
}

impl OpenAIChatModel {
    fn request_body(&self, request: &ModelRequest, stream: bool) -> Result<Value, ModelError> {
        let messages = messages_to_api(&request.messages)?;
        let tools: Vec<_> = request.tools.iter().map(tool_to_api).collect();
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "tools": tools,
            "stream": stream
        });
        if stream {
            body["stream_options"] = json!({ "include_usage": true });
        }
        Ok(body)
    }
}

fn tool_to_api(tool: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.input_schema
        }
    })
}

fn messages_to_api(messages: &[Message]) -> Result<Vec<Value>, ModelError> {
    let mut result = Vec::new();
    for message in messages {
        result.extend(message_to_api(message)?);
    }
    Ok(result)
}

fn message_to_api(message: &Message) -> Result<Vec<Value>, ModelError> {
    match message.role {
        Role::System | Role::User => {
            let role = if message.role == Role::System {
                "system"
            } else {
                "user"
            };
            let mut parts = Vec::new();
            for content in &message.content {
                match content {
                    Content::Text { text } => parts.push(json!({
                        "type": "text",
                        "text": text
                    })),
                    Content::Image { source, detail } if message.role == Role::User => {
                        let mut part = json!({
                            "type": "image_url",
                            "image_url": {
                                "detail": chat_image_detail(*detail)
                            }
                        });
                        match source {
                            ImageSource::Url { url } => {
                                part["image_url"]["url"] = json!(url);
                            }
                            ImageSource::FileId { .. } => {
                                return Err(ModelError::new(
                                    "OpenAI Chat Completions does not support image file IDs",
                                ));
                            }
                        }
                        parts.push(part);
                    }
                    Content::Image { .. } => {
                        return Err(ModelError::new(
                            "OpenAI Chat Completions image input is only supported in user messages",
                        ));
                    }
                    _ => {
                        return Err(ModelError::new(format!(
                            "unsupported content in {role} message"
                        )));
                    }
                }
            }
            Ok(vec![json!({
                "role": role,
                "content": parts
            })])
        }
        Role::Assistant => {
            let mut text_parts = Vec::new();
            let mut tool_calls = Vec::new();

            for content in &message.content {
                match content {
                    Content::Text { text } => text_parts.push(text.clone()),
                    Content::ToolCall {
                        id,
                        name,
                        arguments,
                    } => {
                        tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": serde_json::to_string(arguments).map_err(|error|
                                    ModelError::new(format!("failed to serialize tool arguments: {error}"))
                                )?
                            }
                        }));
                    }
                    Content::ProviderData { provider, data } if provider == PROVIDER_NAME => {
                        return Ok(vec![data.clone()]);
                    }
                    Content::Image { .. } => {
                        return Err(ModelError::new(
                            "image content is not supported in assistant messages",
                        ));
                    }
                    _ => {}
                }
            }

            let mut api_message = json!({ "role": "assistant" });
            if !text_parts.is_empty() {
                api_message["content"] = json!(text_parts.join(""));
            } else if tool_calls.is_empty() {
                api_message["content"] = Value::Null;
            }
            if !tool_calls.is_empty() {
                api_message["tool_calls"] = json!(tool_calls);
            }
            Ok(vec![api_message])
        }
        Role::Tool => {
            let mut parts = Vec::new();
            for content in &message.content {
                if let Content::ToolResult {
                    call_id, result, ..
                } = content
                {
                    parts.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": serde_json::to_string(result).map_err(|error|
                            ModelError::new(format!("failed to serialize tool result: {error}"))
                        )?
                    }));
                }
            }
            Ok(parts)
        }
    }
}

fn chat_image_detail(detail: ImageDetail) -> &'static str {
    match detail {
        ImageDetail::Auto => "auto",
        ImageDetail::Low => "low",
        ImageDetail::High => "high",
        ImageDetail::Original => "auto",
    }
}

fn response_to_model(response: ChatCompletionResponse) -> Result<ModelResponse, ModelError> {
    let choice =
        response.choices.into_iter().next().ok_or_else(|| {
            ModelError::new("OpenAI Chat Completions response contains no choices")
        })?;
    let message = choice.message;

    let mut content = Vec::new();

    let text = message
        .content
        .as_ref()
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty());
    let refusal = message
        .refusal
        .as_ref()
        .filter(|refusal| !refusal.is_empty());

    if let Some(text) = text {
        content.push(Content::Text { text: text.into() });
    } else if let Some(refusal) = refusal {
        content.push(Content::Text {
            text: refusal.into(),
        });
    }

    if let Some(tool_calls) = message.tool_calls {
        for tool_call in tool_calls {
            let arguments =
                serde_json::from_str(&tool_call.function.arguments).map_err(|error| {
                    ModelError::new(format!(
                        "invalid arguments for tool `{}`: {error}",
                        tool_call.function.name
                    ))
                })?;
            content.push(Content::ToolCall {
                id: tool_call.id,
                name: tool_call.function.name,
                arguments,
            });
        }
    }

    let usage = response.usage.unwrap_or_default();
    Ok(ModelResponse {
        message: Message::new(Role::Assistant, content),
        usage: Usage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
        },
    })
}

fn api_status_error(status: StatusCode, body: &str) -> ModelError {
    let message = serde_json::from_str::<ApiErrorEnvelope>(body)
        .ok()
        .map(|error| error.error.message)
        .unwrap_or_else(|| body.to_owned());
    let message = format!("OpenAI API returned {status}: {message}");
    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        ModelError::retryable(message)
    } else {
        ModelError::new(message)
    }
}

#[derive(Default)]
struct StreamAccumulator {
    text: String,
    tool_calls: Vec<Option<PartialToolCall>>,
    usage: Option<Usage>,
}

#[derive(Default)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl StreamAccumulator {
    async fn apply_delta(
        &mut self,
        delta: Delta,
        event_sink: &dyn ModelEventSink,
    ) -> Result<(), ModelError> {
        if let Some(text) = delta.content.filter(|text| !text.is_empty()) {
            event_sink
                .emit(ModelEvent::TextDelta { text: text.clone() })
                .await;
            self.text.push_str(&text);
        }

        if let Some(delta_tool_calls) = delta.tool_calls {
            for delta_tool_call in delta_tool_calls {
                let index = delta_tool_call.index;
                if self.tool_calls.len() <= index {
                    self.tool_calls.resize_with(index + 1, Default::default);
                }
                let partial = self.tool_calls[index].get_or_insert_with(Default::default);
                if let Some(id) = delta_tool_call.id {
                    partial.id = Some(id);
                }
                if let Some(function) = delta_tool_call.function {
                    if let Some(name) = function.name {
                        partial.name = Some(name);
                    }
                    if let Some(arguments) = function.arguments {
                        partial.arguments.push_str(&arguments);
                    }
                }
            }
        }

        Ok(())
    }

    fn into_response(self) -> Result<ModelResponse, ModelError> {
        let StreamAccumulator {
            text,
            tool_calls,
            usage,
        } = self;

        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(Content::Text { text });
        }

        for (index, partial) in tool_calls.into_iter().enumerate() {
            let partial = partial.ok_or_else(|| {
                ModelError::new(format!(
                    "OpenAI stream ended with incomplete tool call at index {index}"
                ))
            })?;
            let id = partial.id.ok_or_else(|| {
                ModelError::new(format!(
                    "OpenAI stream ended with tool call missing id at index {index}"
                ))
            })?;
            let name = partial.name.ok_or_else(|| {
                ModelError::new(format!(
                    "OpenAI stream ended with tool call missing name at index {index}"
                ))
            })?;
            let arguments = serde_json::from_str(&partial.arguments).map_err(|error| {
                ModelError::new(format!(
                    "invalid arguments for tool `{name}` from stream: {error}"
                ))
            })?;
            content.push(Content::ToolCall {
                id,
                name,
                arguments,
            });
        }

        Ok(ModelResponse {
            message: Message::new(Role::Assistant, content),
            usage: usage.unwrap_or_default(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<Choice>,
    usage: Option<ChatUsage>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: AssistantMessage,
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    content: Option<Value>,
    tool_calls: Option<Vec<ToolCall>>,
    refusal: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ToolCall {
    id: String,
    #[serde(rename = "type")]
    _kind: String,
    function: FunctionCall,
}

#[derive(Debug, Deserialize)]
struct FunctionCall {
    name: String,
    arguments: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    usage: Option<ChatUsage>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: Delta,
    #[serde(rename = "finish_reason")]
    _finish_reason: Option<Value>,
}

#[derive(Debug, Deserialize, Default)]
struct Delta {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<DeltaToolCall>>,
}

#[derive(Debug, Deserialize)]
struct DeltaToolCall {
    index: usize,
    id: Option<String>,
    #[serde(rename = "type")]
    _kind: Option<String>,
    function: Option<DeltaFunction>,
}

#[derive(Debug, Deserialize)]
struct DeltaFunction {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiErrorEnvelope {
    error: ApiError,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_system_message_to_content_array() {
        let messages = vec![Message::system("Answer briefly.")];
        let input = messages_to_api(&messages).unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "system");
        let content = input[0]["content"].as_array().unwrap();
        assert_eq!(
            content[0],
            json!({ "type": "text", "text": "Answer briefly." })
        );
    }

    #[test]
    fn converts_user_message_with_text_and_image_url() {
        let messages = vec![Message::user_content(vec![
            Content::text("Compare these images"),
            Content::image_url_with_detail("https://example.com/image.png", ImageDetail::Low),
        ])];
        let input = messages_to_api(&messages).unwrap();
        let content = input[0]["content"].as_array().unwrap();
        assert_eq!(
            content[0],
            json!({ "type": "text", "text": "Compare these images" })
        );
        assert_eq!(
            content[1],
            json!({
                "type": "image_url",
                "image_url": {
                    "url": "https://example.com/image.png",
                    "detail": "low"
                }
            })
        );
    }

    #[test]
    fn rejects_image_file_id() {
        let messages = vec![Message::user_content(vec![Content::image_file("file-123")])];
        let result = messages_to_api(&messages);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .message
                .contains("does not support image file IDs")
        );
    }

    #[test]
    fn maps_original_image_detail_to_auto() {
        let messages = vec![Message::user_content(vec![Content::image_url_with_detail(
            "https://example.com/image.png",
            ImageDetail::Original,
        )])];
        let input = messages_to_api(&messages).unwrap();
        let content = input[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["image_url"]["detail"], "auto");
    }

    #[test]
    fn converts_assistant_message_with_text_and_tool_call() {
        let messages = vec![Message::new(
            Role::Assistant,
            vec![
                Content::Text {
                    text: "Let me calculate that.".into(),
                },
                Content::ToolCall {
                    id: "call-1".into(),
                    name: "add".into(),
                    arguments: json!({"left": 20, "right": 22}),
                },
            ],
        )];
        let input = messages_to_api(&messages).unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "assistant");
        assert_eq!(input[0]["content"], "Let me calculate that.");
        let tool_calls = input[0]["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls[0]["id"], "call-1");
        assert_eq!(tool_calls[0]["type"], "function");
        assert_eq!(tool_calls[0]["function"]["name"], "add");
        assert_eq!(
            tool_calls[0]["function"]["arguments"],
            "{\"left\":20,\"right\":22}"
        );
    }

    #[test]
    fn converts_tool_result_message() {
        let messages = vec![Message::new(
            Role::Tool,
            vec![Content::ToolResult {
                call_id: "call-1".into(),
                result: json!({ "sum": 42 }),
                is_error: false,
            }],
        )];
        let input = messages_to_api(&messages).unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "tool");
        assert_eq!(input[0]["tool_call_id"], "call-1");
        assert_eq!(input[0]["content"], "{\"sum\":42}");
    }

    #[test]
    fn parses_text_response() {
        let response = ChatCompletionResponse {
            choices: vec![Choice {
                message: AssistantMessage {
                    content: Some(Value::String("Hello!".into())),
                    tool_calls: None,
                    refusal: None,
                },
            }],
            usage: Some(ChatUsage {
                prompt_tokens: 10,
                completion_tokens: 2,
            }),
        };
        let model_response = response_to_model(response).unwrap();
        assert_eq!(model_response.message.text_content(), "Hello!");
        assert_eq!(model_response.usage.input_tokens, 10);
        assert_eq!(model_response.usage.output_tokens, 2);
    }

    #[test]
    fn parses_tool_call_response() {
        let response = ChatCompletionResponse {
            choices: vec![Choice {
                message: AssistantMessage {
                    content: Some(Value::Null),
                    tool_calls: Some(vec![ToolCall {
                        id: "call-1".into(),
                        _kind: "function".into(),
                        function: FunctionCall {
                            name: "add".into(),
                            arguments: "{\"left\":20,\"right\":22}".into(),
                        },
                    }]),
                    refusal: None,
                },
            }],
            usage: Some(ChatUsage {
                prompt_tokens: 15,
                completion_tokens: 8,
            }),
        };
        let model_response = response_to_model(response).unwrap();
        assert!(
            model_response
                .message
                .content
                .iter()
                .any(|content| matches!(
                    content,
                    Content::ToolCall { name, .. } if name == "add"
                ))
        );
    }

    #[test]
    fn parses_refusal_as_text() {
        let response = ChatCompletionResponse {
            choices: vec![Choice {
                message: AssistantMessage {
                    content: Some(Value::Null),
                    tool_calls: None,
                    refusal: Some("I cannot answer that.".into()),
                },
            }],
            usage: None,
        };
        let model_response = response_to_model(response).unwrap();
        assert_eq!(
            model_response.message.text_content(),
            "I cannot answer that."
        );
    }

    #[tokio::test]
    async fn accumulator_combines_text_and_tool_call_deltas() {
        let deltas = vec![
            Delta {
                content: Some("The ".into()),
                tool_calls: None,
            },
            Delta {
                content: Some("answer ".into()),
                tool_calls: None,
            },
            Delta {
                content: Some("is ".into()),
                tool_calls: None,
            },
            Delta {
                content: None,
                tool_calls: Some(vec![DeltaToolCall {
                    index: 0,
                    id: Some("call-1".into()),
                    _kind: Some("function".into()),
                    function: Some(DeltaFunction {
                        name: Some("add".into()),
                        arguments: Some("{\"left\":".into()),
                    }),
                }]),
            },
            Delta {
                content: None,
                tool_calls: Some(vec![DeltaToolCall {
                    index: 0,
                    id: None,
                    _kind: None,
                    function: Some(DeltaFunction {
                        name: None,
                        arguments: Some("20,\"right\":22}".into()),
                    }),
                }]),
            },
        ];

        let mut accumulator = StreamAccumulator::default();
        let sink = TestEventSink::default();
        for delta in deltas {
            accumulator.apply_delta(delta, &sink).await.unwrap();
        }
        let response = accumulator.into_response().unwrap();
        assert_eq!(response.message.text_content(), "The answer is ");
        assert!(response.message.content.iter().any(|content| matches!(
            content,
            Content::ToolCall { id, name, .. } if id == "call-1" && name == "add"
        )));
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 3);
    }

    #[derive(Default)]
    struct TestEventSink {
        events: std::sync::Mutex<Vec<ModelEvent>>,
    }

    #[async_trait]
    impl ModelEventSink for TestEventSink {
        async fn emit(&self, event: ModelEvent) {
            self.events.lock().unwrap().push(event);
        }
    }
}
