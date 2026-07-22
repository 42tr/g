# g

`g` 是一个轻量的 Rust Agent 运行时。它负责在模型、工具和子 Agent 之间驱动多轮对话，并提供流式事件、工具调用策略、运行限额、取消以及图像输入等基础能力。

目前内置的模型适配器基于 OpenAI Responses API；同时也可以通过实现 `Model` trait 接入其他模型。

## 功能

- 使用 `Agent` 构建并运行单 Agent 工作流
- 通过 `#[tool]` 将异步 Rust 函数声明为模型工具，并自动生成 JSON Schema
- 支持工具调用、错误回传和调用前授权策略
- 支持将任务 handoff 给具名子 Agent
- 支持文本增量及运行生命周期事件
- 支持 URL、data URL 和 OpenAI file ID 形式的图像输入
- 支持最大轮数、最大工具调用数、超时和主动取消
- 通过 `Model`、`Tool`、`Policy` 和 `EventSink` trait 扩展运行时

## 环境要求

- Rust 2024 edition 对应的工具链
- 可用的 OpenAI API Key，或兼容 OpenAI Responses API 的服务

配置环境变量：

```bash
export OPENAI_API_KEY="your-api-key"
# 可选，默认值为 gpt-5.6
export OPENAI_MODEL="gpt-5.6"
# 可选，默认值为 https://api.openai.com/v1
export OPENAI_BASE_URL="https://api.openai.com/v1"
```

`OPENAI_BASE_URL` 应指向 API 的 `/v1` 根路径，运行时会向其下的 `/responses` 发起请求。

## 快速开始

克隆仓库后，可以直接运行内置示例：

```bash
cargo run --example calculator
```

在其他项目中使用当前 Git 版本：

```toml
[dependencies]
g = { git = "https://github.com/42tr/g.git" }
futures-util = "0.3"
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt"] }
```

下面的例子声明了一个加法工具，并以流式方式输出模型回复：

```rust
use std::{
    io::{self, Write},
    sync::Arc,
};

use futures_util::StreamExt;
use g::{tool, Agent, OpenAIModel, RunEvent, ToolCallError};
use serde_json::{json, Value};

#[tool(name = "add", description = "Add two integers")]
async fn add(left: i64, right: i64) -> Result<Value, ToolCallError> {
    Ok(json!({ "sum": left + right }))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let agent = Agent::new(Arc::new(OpenAIModel::from_env()?))
        .tools([add])
        .instruction("Always use the add tool for addition questions.");

    let mut events = agent.stream_run("What is 20 + 22?");
    while let Some(event) = events.next().await {
        match event? {
            RunEvent::TextDelta { text, .. } => {
                print!("{text}");
                io::stdout().flush()?;
            }
            RunEvent::Completed { .. } => println!(),
            _ => {}
        }
    }

    Ok(())
}
```

不需要增量输出时，使用 `run`：

```rust
let output = agent.run("What is 20 + 22?").await?;

println!("{}", output.final_text);
println!("turns: {}, tool calls: {}", output.turns, output.tool_calls);
println!(
    "tokens: {} input / {} output",
    output.usage.input_tokens,
    output.usage.output_tokens
);
```

## 工具

`#[tool]` 仅支持异步自由函数。函数参数会被转换为工具的 JSON Schema 和调用参数；`Option<T>` 参数不会出现在 `required` 列表中。函数必须返回 `Result<T, E>`，其中输出需要可序列化，错误需要实现 `Display`。

```rust
#[tool(name = "lookup", description = "Look up recent market values")]
async fn lookup(
    market: String,
    days: Option<u32>,
) -> Result<Value, ToolCallError> {
    Ok(json!({ "market": market, "days": days }))
}
```

需要完整控制 schema、行为元数据或调用上下文时，可以直接实现 `Tool` trait。`ToolContext` 包含本次运行的 `run_id` 和 `CancellationToken`。

工具执行失败时，错误会作为带有 `is_error: true` 的工具结果返回给模型，由模型决定如何继续；策略拒绝则会直接结束本次运行。

## Handoff

父 Agent 可以把任务委派给具名子 Agent。子 Agent 必须设置非空且不重复的 `name`；运行时会为其生成 `handoff_to_<name>` 工具。

```rust
let model = Arc::new(OpenAIModel::from_env()?);

let math_agent = Agent::new(model.clone())
    .name("math")
    .description("Solve arithmetic questions")
    .tools([add])
    .instruction("Use the available tools and return a concise answer.");

let agent = Agent::new(model)
    .handoff([math_agent])
    .instruction("Hand off arithmetic questions to the math agent.");

let output = agent.run("What is 20 + 22?").await?;
println!("{}", output.final_text);
```

Handoff 会继承父运行的取消信号和事件接收器。子 Agent 的 token 用量会累加到父运行结果中。

## 图像输入

将文本和图像内容组合后直接传给 `run` 或 `stream_run`：

```rust
use g::{Content, ImageDetail};

let prompt = [
    Content::text("Describe this image in one sentence."),
    Content::image_url_with_detail(
        "https://example.com/image.jpg",
        ImageDetail::Low,
    ),
];

let output = agent.run(prompt).await?;
```

可用的图像构造方法包括 `Content::image_url`、`Content::image_url_with_detail`、`Content::image_file` 和 `Content::image_file_with_detail`。图像目前只支持出现在用户消息中。

## 策略、事件和运行限制

默认策略是 `AllowAll`。可以实现 `Policy` 并通过 `with_policy` 在每次工具执行前检查工具及其参数：

```rust
let agent = Agent::new(model).with_policy(Arc::new(MyPolicy));
```

`stream_run` 会返回以下 `RunEvent`：

- `Started` / `Completed`
- `ModelStarted` / `ModelCompleted`
- `TextDelta`
- `ToolStarted` / `ToolCompleted`
- `HandoffStarted` / `HandoffCompleted`

也可以实现 `EventSink`，再通过 `with_event_sink` 将事件发送到日志、指标或审计系统。

默认运行限制为 16 轮、32 次工具调用和 120 秒超时：

```rust
use std::time::Duration;
use g::RunLimits;

let agent = Agent::new(model).with_limits(RunLimits {
    max_turns: 8,
    max_tool_calls: 16,
    timeout: Duration::from_secs(60),
});
```

如需主动取消，使用 `Runtime::run` 和带 `CancellationToken` 的 `RunRequest`。丢弃 `stream_run` 返回的事件流后，运行也会在下一次发送事件时收到取消信号。

## 示例

```bash
# 工具调用与流式输出
cargo run --example calculator

# 图像理解
cargo run --example vision

# Agent handoff
cargo run --example handoff
```

设置 `RUST_LOG` 可以查看运行日志：

```bash
RUST_LOG=g=debug cargo run --example calculator
```

## 开发

```bash
cargo fmt --check
cargo test --workspace
cargo clippy --workspace --all-targets
```

当前实现按顺序执行同一轮中的多个工具调用；`ToolBehavior` 已提供 `read_only`、`idempotent` 和 `parallel_safe` 元数据，但运行时暂未据此并行调度。
