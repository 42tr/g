use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Content, Message, Model, ModelError, ModelRequest, ModelResponse, Role, ToolSpec, Usage,
};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "gpt-5.6";
const PROVIDER_NAME: &str = "openai";

#[derive(Clone, Debug)]
pub struct OpenAIModel {
    client: Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl OpenAIModel {
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
impl Model for OpenAIModel {
    async fn generate(&self, request: ModelRequest) -> Result<ModelResponse, ModelError> {
        tracing::debug!(
            model = %self.model,
            messages = request.messages.len(),
            tools = request.tools.len(),
            "sending OpenAI Responses API request"
        );
        let input = messages_to_input(&request.messages)?;
        let tools: Vec<_> = request.tools.iter().map(tool_to_api).collect();
        let body = json!({
            "model": self.model,
            "input": input,
            "tools": tools,
            "store": false
        });

        let response = self
            .client
            .post(format!("{}/responses", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|error| ModelError::retryable(error.to_string()))?;
        let status = response.status();
        tracing::debug!(%status, model = %self.model, "received OpenAI Responses API response");
        let body = response
            .text()
            .await
            .map_err(|error| ModelError::retryable(error.to_string()))?;

        if !status.is_success() {
            tracing::warn!(%status, model = %self.model, "OpenAI Responses API request failed");
            return Err(api_status_error(status, &body));
        }

        let response: ApiResponse = serde_json::from_str(&body)
            .map_err(|error| ModelError::new(format!("invalid OpenAI response: {error}")))?;
        let response = response_to_model(response)?;
        tracing::debug!(
            model = %self.model,
            input_tokens = response.usage.input_tokens,
            output_tokens = response.usage.output_tokens,
            "parsed OpenAI model response"
        );
        Ok(response)
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
                input.push(json!({
                    "role": role,
                    "content": message.text_content()
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
                            "arguments": serde_json::to_string(arguments).map_err(|error|
                                ModelError::new(format!("failed to serialize tool arguments: {error}"))
                            )?
                        })),
                        Content::ToolResult { .. } | Content::ProviderData { .. } => {}
                    }
                }
            }
            Role::Tool => {
                for content in &message.content {
                    if let Content::ToolResult {
                        call_id, result, ..
                    } = content
                    {
                        input.push(json!({
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": serde_json::to_string(result).map_err(|error|
                                ModelError::new(format!("failed to serialize tool result: {error}"))
                            )?
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
                let arguments = serde_json::from_str(&encoded_arguments).map_err(|error| {
                    ModelError::new(format!("invalid arguments for tool `{name}`: {error}"))
                })?;
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
        },
    })
}

fn required_string(item: &Value, field: &str) -> Result<String, ModelError> {
    item.get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ModelError::new(format!("OpenAI output is missing `{field}`")))
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
struct ApiUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
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
}
