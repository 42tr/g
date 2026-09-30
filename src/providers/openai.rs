use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

use super::common::{
    ApiError, OpenAIClient, encode_tool_arguments, encode_tool_result, parse_tool_arguments,
};
use crate::{
    Content, ImageSource, Message, Model, ModelError, ModelEvent, ModelEventSink, ModelRequest,
    ModelResponse, Role, ToolSpec, Usage,
};

const DEFAULT_MODEL: &str = "gpt-5.6";
const PROVIDER_NAME: &str = "openai";

#[derive(Clone, Debug)]
pub struct OpenAIModel {
    client: OpenAIClient,
}

impl OpenAIModel {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: OpenAIClient::new(api_key.into(), model.into()),
        }
    }

    pub fn from_env() -> Result<Self, ModelError> {
        Ok(Self {
            client: OpenAIClient::from_env(DEFAULT_MODEL)?,
        })
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.client.set_base_url(base_url.into());
        self
    }

    /// Replace the default HTTP client (30s connect timeout, 300s read timeout).
    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.client.set_http_client(client);
        self
    }
}

#[async_trait]
impl Model for OpenAIModel {
    async fn generate(&self, request: ModelRequest) -> Result<ModelResponse, ModelError> {
        tracing::debug!(
            model = %self.client.model,
            messages = request.messages.len(),
            tools = request.tools.len(),
            "sending OpenAI Responses API request"
        );
        let body = self.request_body(&request, false)?;
        let response: ApiResponse = self.client.post_json("/responses", &body).await?;
        let response = response_to_model(response)?;
        tracing::debug!(
            model = %self.client.model,
            input_tokens = response.usage.input_tokens,
            output_tokens = response.usage.output_tokens,
            "parsed OpenAI model response"
        );
        Ok(response)
    }

    async fn generate_stream(
        &self,
        request: ModelRequest,
        event_sink: &dyn ModelEventSink,
    ) -> Result<ModelResponse, ModelError> {
        tracing::debug!(
            model = %self.client.model,
            messages = request.messages.len(),
            tools = request.tools.len(),
            "opening OpenAI Responses API stream"
        );
        let body = self.request_body(&request, true)?;
        let mut events = self.client.post_sse("/responses", &body).await?;
        while let Some(data) = events.next().await {
            let data = data?;
            if data == "[DONE]" {
                break;
            }
            let payload: Value = serde_json::from_str(&data).map_err(|error| {
                ModelError::new(format!("invalid OpenAI stream event: {error}"))
            })?;
            match payload.get("type").and_then(Value::as_str) {
                Some("response.output_text.delta" | "response.refusal.delta") => {
                    if let Some(text) = payload.get("delta").and_then(Value::as_str) {
                        event_sink
                            .emit(ModelEvent::TextDelta { text: text.into() })
                            .await;
                    }
                }
                // `response_to_model` turns failed and incomplete responses into errors.
                Some("response.completed" | "response.failed" | "response.incomplete") => {
                    let response = payload.get("response").cloned().ok_or_else(|| {
                        ModelError::new("OpenAI terminal event is missing `response`")
                    })?;
                    let response: ApiResponse =
                        serde_json::from_value(response).map_err(|error| {
                            ModelError::new(format!("invalid OpenAI terminal event: {error}"))
                        })?;
                    return response_to_model(response);
                }
                Some("error") => {
                    let message = payload
                        .pointer("/error/message")
                        .or_else(|| payload.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown streaming error");
                    return Err(ModelError::new(format!("OpenAI stream error: {message}")));
                }
                _ => {}
            }
        }

        Err(ModelError::retryable(
            "OpenAI stream ended before a completed response",
        ))
    }
}

impl OpenAIModel {
    fn request_body(&self, request: &ModelRequest, stream: bool) -> Result<Value, ModelError> {
        let input = messages_to_input(&request.messages)?;
        let mut body = json!({
            "model": self.client.model,
            "input": input,
            "store": false,
            // With `store: false` the server keeps no reasoning state, so replayed
            // reasoning items must carry their encrypted content.
            "include": ["reasoning.encrypted_content"],
            "stream": stream
        });
        // Some compatible servers reject an empty `tools` array.
        if !request.tools.is_empty() {
            body["tools"] = request.tools.iter().map(tool_to_api).collect();
        }
        Ok(body)
    }
}

fn tool_to_api(tool: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.input_schema
    })
}

fn messages_to_input(messages: &[Message]) -> Result<Vec<Value>, ModelError> {
    let mut input = Vec::new();
    for message in messages {
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
                            "type": "input_text",
                            "text": text
                        })),
                        Content::Image { source, detail } if message.role == Role::User => {
                            let mut part = json!({
                                "type": "input_image",
                                "detail": detail.as_str()
                            });
                            match source {
                                ImageSource::Url { url } => part["image_url"] = json!(url),
                                ImageSource::FileId { file_id } => part["file_id"] = json!(file_id),
                            }
                            parts.push(part);
                        }
                        Content::Image { .. } => {
                            return Err(ModelError::new(
                                "OpenAI image input is only supported in user messages",
                            ));
                        }
                        _ => {
                            return Err(ModelError::new(format!(
                                "unsupported content in {role} message"
                            )));
                        }
                    }
                }
                input.push(json!({
                    "role": role,
                    "content": parts
                }));
            }
            Role::Assistant => {
                let provider_items: Vec<_> = message
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        Content::ProviderData { provider, data } if provider == PROVIDER_NAME => {
                            Some(data.clone())
                        }
                        _ => None,
                    })
                    .collect();
                if !provider_items.is_empty() {
                    input.extend(provider_items);
                    continue;
                }

                for content in &message.content {
                    match content {
                        Content::Text { text } => input.push(json!({
                            "role": "assistant",
                            "content": text
                        })),
                        Content::ToolCall {
                            id,
                            name,
                            arguments,
                        } => input.push(json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": encode_tool_arguments(arguments)?
                        })),
                        Content::Image { .. } => {
                            return Err(ModelError::new(
                                "image content is not supported in assistant messages",
                            ));
                        }
                        Content::ToolResult { .. } | Content::ProviderData { .. } => {}
                    }
                }
            }
            Role::Tool => {
                for content in &message.content {
                    if let Content::ToolResult {
                        call_id,
                        result,
                        is_error,
                    } = content
                    {
                        input.push(json!({
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": encode_tool_result(result, *is_error)?
                        }));
                    }
                }
            }
        }
    }
    Ok(input)
}

fn response_to_model(response: ApiResponse) -> Result<ModelResponse, ModelError> {
    if let Some(error) = response.error {
        return Err(ModelError::new(error.message));
    }
    if response.status.as_deref() != Some("completed") {
        let status = response.status.as_deref().unwrap_or("unknown");
        let details = response
            .incomplete_details
            .map(|details| format!(": {details}"))
            .unwrap_or_default();
        return Err(ModelError::new(format!(
            "OpenAI response ended with status `{status}`{details}"
        )));
    }

    let mut content = Vec::new();
    for item in response.output {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        content.push(Content::ProviderData {
            provider: PROVIDER_NAME.into(),
            data: item.clone(),
        });

        match item_type {
            "message" => {
                if let Some(parts) = item.get("content").and_then(Value::as_array) {
                    for part in parts {
                        if part.get("type").and_then(Value::as_str) == Some("output_text")
                            && let Some(text) = part.get("text").and_then(Value::as_str)
                        {
                            content.push(Content::Text { text: text.into() });
                        } else if part.get("type").and_then(Value::as_str) == Some("refusal")
                            && let Some(text) = part.get("refusal").and_then(Value::as_str)
                        {
                            content.push(Content::Text { text: text.into() });
                        }
                    }
                }
            }
            "function_call" => {
                let call_id = required_string(&item, "call_id")?;
                let name = required_string(&item, "name")?;
                let encoded_arguments = required_string(&item, "arguments")?;
                let arguments = parse_tool_arguments(&name, &encoded_arguments)?;
                content.push(Content::ToolCall {
                    id: call_id,
                    name,
                    arguments,
                });
            }
            _ => {}
        }
    }

    let usage = response.usage.unwrap_or_default();
    Ok(ModelResponse {
        message: Message::new(Role::Assistant, content),
        usage: Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cached_input_tokens: usage.input_tokens_details.unwrap_or_default().cached_tokens,
            reasoning_tokens: usage
                .output_tokens_details
                .unwrap_or_default()
                .reasoning_tokens,
        },
    })
}

fn required_string(item: &Value, field: &str) -> Result<String, ModelError> {
    item.get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ModelError::new(format!("OpenAI output is missing `{field}`")))
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    status: Option<String>,
    #[serde(default)]
    output: Vec<Value>,
    usage: Option<ApiUsage>,
    error: Option<ApiError>,
    incomplete_details: Option<Value>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(default)]
struct ApiUsage {
    input_tokens: u64,
    output_tokens: u64,
    // Options: some compatible servers send explicit nulls.
    input_tokens_details: Option<InputTokensDetails>,
    output_tokens_details: Option<OutputTokensDetails>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(default)]
struct InputTokensDetails {
    cached_tokens: u64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(default)]
struct OutputTokensDetails {
    reasoning_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_function_calls_and_preserves_provider_items() {
        let response = ApiResponse {
            status: Some("completed".into()),
            output: vec![
                json!({ "type": "reasoning", "id": "reasoning-1", "summary": [] }),
                json!({
                    "type": "function_call",
                    "call_id": "call-1",
                    "name": "add",
                    "arguments": "{\"left\":20,\"right\":22}"
                }),
            ],
            usage: Some(ApiUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }),
            error: None,
            incomplete_details: None,
        };

        let model_response = response_to_model(response).unwrap();
        assert_eq!(model_response.usage.input_tokens, 10);
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

        let replay = messages_to_input(&[model_response.message]).unwrap();
        assert_eq!(replay.len(), 2);
        assert_eq!(replay[0]["type"], "reasoning");
        assert_eq!(replay[1]["type"], "function_call");
    }

    #[test]
    fn converts_tool_results_to_function_call_outputs() {
        let messages = vec![Message::new(
            Role::Tool,
            vec![Content::ToolResult {
                call_id: "call-1".into(),
                result: json!({ "sum": 42 }),
                is_error: false,
            }],
        )];

        let input = messages_to_input(&messages).unwrap();
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["call_id"], "call-1");
        assert_eq!(input[0]["output"], "{\"sum\":42}");
    }

    #[test]
    fn marks_failed_tool_results_and_accepts_empty_arguments() {
        let messages = vec![Message::new(
            Role::Tool,
            vec![Content::ToolResult {
                call_id: "call-1".into(),
                result: json!("boom"),
                is_error: true,
            }],
        )];
        let input = messages_to_input(&messages).unwrap();
        assert_eq!(input[0]["output"], r#"{"error":"boom"}"#);

        let response = ApiResponse {
            status: Some("completed".into()),
            output: vec![json!({
                "type": "function_call",
                "call_id": "call-1",
                "name": "ping",
                "arguments": ""
            })],
            usage: None,
            error: None,
            incomplete_details: None,
        };
        let message = response_to_model(response).unwrap().message;
        assert!(message.content.iter().any(|content| matches!(
            content,
            Content::ToolCall { arguments, .. } if *arguments == json!({})
        )));
    }

    #[test]
    fn requests_encrypted_reasoning_and_omits_empty_tools() {
        let model = OpenAIModel::new("key", "model");
        let body = model
            .request_body(
                &ModelRequest::new(vec![Message::user("hi")], Vec::new()),
                false,
            )
            .unwrap();
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn converts_url_data_url_and_file_id_image_inputs() {
        let messages = vec![Message::user_content(vec![
            Content::text("Compare these images"),
            Content::image_url_with_detail(
                "https://example.com/image.png",
                crate::ImageDetail::Low,
            ),
            Content::image_url("data:image/png;base64,aGVsbG8="),
            Content::image_file_with_detail("file-123", crate::ImageDetail::Original),
        ])];

        let input = messages_to_input(&messages).unwrap();
        let parts = input[0]["content"].as_array().unwrap();
        assert_eq!(
            parts[0],
            json!({ "type": "input_text", "text": "Compare these images" })
        );
        assert_eq!(
            parts[1],
            json!({
                "type": "input_image",
                "image_url": "https://example.com/image.png",
                "detail": "low"
            })
        );
        assert_eq!(parts[2]["image_url"], "data:image/png;base64,aGVsbG8=");
        assert_eq!(
            parts[3],
            json!({
                "type": "input_image",
                "file_id": "file-123",
                "detail": "original"
            })
        );
    }
}
