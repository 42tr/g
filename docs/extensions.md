# 按请求配置 MCP 与 Skills

启用可选 feature；原有本地 Tool 无需修改：

```toml
g = { git = "https://github.com/42tr/g.git", features = ["mcp", "skills"] }
```

g 不扫描目录，不读取 MCP/skills 配置文件，不监听文件变化。调用方构造 `ExtensionConfig` 或自行反序列化 JSON，随每个 `RunRequest` 传入。省略配置或传空列表表示本次禁用；不会继承上一个请求的能力。

## 使用

```rust
use std::sync::Arc;
use g::{Agent, ExtensionConfig, Message, RunRequest, Runtime};
use g::mcp::{McpManager, McpServerConfig};
use g::skills::{SkillDefinition, SkillResource};

// model 由宿主提供；manager 可供多个 Agent、多个并发请求共享。
let agent = Agent::new(model);
let manager = Arc::new(McpManager::anonymous());
let runtime = Runtime::new().with_mcp_manager(manager.clone());

let mut server = McpServerConfig::http("tasks", "http://127.0.0.1:8080/mcp");
server.allowed_tools = vec!["get_task".into()];
let mut skill = SkillDefinition::new(
    "task-analysis", "分析任务进度", "读取任务数据，按 references/state.md 解释状态。",
);
skill.resources.insert("references/state.md".into(), SkillResource::text("状态字段说明"));
let config = Arc::new(ExtensionConfig { mcp: vec![server], skills: vec![skill] });
config.validate()?;
let request = RunRequest::new(vec![Message::user("分析任务 42")])
    .with_extensions(config);
let result = runtime.run(&agent, request).await;
let closed = manager.close().await; // 失败路径同样关闭；服务端在应用退出时才关闭。
let output = result?;
closed?;
```

只开启 `skills` 时不需要 manager，ExtensionConfig 也没有 mcp 字段；只开启 `mcp` 时没有 skills 字段。serde 拒绝未启用功能对应的未知字段，避免配置被静默忽略。可运行例子见 `examples/extensions.rs`，它由宿主显式读取 `G_EXTENSIONS_JSON`，使用现有模型环境变量。

`runtime.stream_run(&agent, request)` 接收同样的请求配置；丢弃返回流会取消本次运行。`runtime.warmup(&agent, request).await` 校验、连接并发现工具，返回 tools/diagnostics，不调用模型或执行业务工具。预热不是发布配置，下次 run 仍需传入配置。准备和预热均受 Agent 的总运行超时约束。

## Skills

配置直接包含 name、description、instructions 和 resources。资源是 `BTreeMap<String, SkillResource>`，首版 content 为 UTF-8 String；`references/state.md` 是虚拟键，不是磁盘路径。绝对路径、`..`、未知键都不会触发文件或网络回退。只读取配置中的文本，不执行脚本。

如已有 SKILL.md 内容，使用 `SkillDefinition::from_markdown(text, resources)` 在内存解析 frontmatter；资源仍需显式提供。重复技能名、无效配置在调用模型前失败。

模型初始只看到技能元数据索引：

- `skills_list({query?, cursor?, limit?})` 查询当前技能，游标绑定快照及查询。
- `skills_read({name, path?, offset?, limit?})` 读取正文或资源。offset/limit 以 Unicode 字符计数，返回 next_offset 支持分页。
- `RunRequest::with_selected_skills(["task-analysis"])` 显式加载技能正文，同样经过 Policy；大正文可能分页，模型可按 next_offset 继续读取。

首版硬预算：512 个技能、每正文/资源 64 KiB、每技能最多 256 个资源、配置序列化合计 8 MiB、每 run 技能上下文 256 KiB。初始索引最多 64 项/约 32 KiB；多出的元数据可通过 skills_list 读取。资源类型首版支持 text/*、application/json、application/yaml。

required_tools 使用 `ToolOrigin::Mcp { server_id, tool_name }` 检查工具是否已配置且存在；不扩张权限。依赖缺失的普通技能不加入索引并产生诊断；显式选择该技能使准备失败。动态 Policy 仍在每次实际调用时校验。

## MCP

支持 stdio 和 Streamable HTTP（JSON/SSE 由 rmcp 处理），每次 run 分页获取 tools/list，不依赖陈旧工具列表。工具名为 `mcp__server__tool`；特殊字符或长名称使用稳定摘要别名，重名报错。`Policy::authorize_extension` 同时获得原始 ToolOrigin 与 InvocationScope，现有 Policy 默认继续调用 authorize。

stdio 传 `McpTransport::Stdio { command, args, cwd, env }`，command/cwd 必须是绝对路径。按 argv 启动，不经过 shell；不继承进程环境，只传配置 env 及 provider 的 env。stderr 默认丢弃，避免服务器将凭证写入宿主日志。

匿名连接使用 `McpManager::anonymous()`。认证连接通过 `McpManager::new(options, Arc<dyn CredentialProvider>)` 创建；请求通过 with_scope 提供身份。provider.resolve(scope, credential_ref) 返回 McpCredentials，支持 bearer_token、自定义 HTTP headers、stdio env 和 generation。非匿名凭证必须有非空 scope.id；这些凭证不交给模型。HTTP 不自动跟随重定向。

连接按服务器、实际连接配置、scope、凭证内容和 generation 分区。不同用户不会共用认证连接。修改配置时不需要重建 manager，旧连接由租约与空闲 TTL 回收；manager 还限制跨版本总连接数和在途调用数。首次连接并发合并，取消一个等待者不影响其他等待者。

`allowed_tools=[]` 不暴露任何远端工具；显式设置 `allow_all_tools=true` 才全部暴露，两者不能同时配置。required server 初始化失败使本次 run 失败；optional server 失败在 context_manifest.diagnostics 中报告降级。没有 manager 时 MCP 配置始终报错。配置本身无效时返回 `AgentError::InvalidConfiguration`；配置有效但运行时失败（连接、凭证解析、发现、manager 已关闭等）返回 `AgentError::Extension`。handoff 子 run 复用父 run 发现的工具，不再重新发现。

远端输入/结构化输出按 JSON Schema 校验，编译后的校验器按 schema 指纹在 manager 内缓存；外部 schema 引用不支持，不访问文件或网络。MCP isError 保留在 ToolResult 的 is_error 中，工具结果保留文本与 structured_content。传输失败、超时等返回稳定错误代码及 execution_state；提交后失败状态为 unknown，不自动重放。非文本内容报告 unsupported_content。

取消/超时会通知远端并进行有界等待；无法确认结束时关闭该连接，共享连接上的其他请求也可能失败。取消不代表远端操作已回滚。排队期间取消不发 tools/call。close 幂等，应用应停止接收请求、等待/取消 run 后调用 close；关闭后的 manager 不再接收新连接。

首版未实现 OAuth 交互、sampling、elicitation、MCP resources/prompts、任务协议、多模态工具输出。发现/结果预算在 SDK 解码后校验，不是传输层字节上限。

## 更新、续聊与兼容性

每次调用传完整快照。宿主可以运行中生成新 Arc<ExtensionConfig>，交给新请求；旧 run 使用旧正文、资源和工具表。Arc 中为普通拥有所有权的数据，g 没有读取共享可变“当前配置”的接口。handoff 继承父请求快照，权限由父子 Policy 共同约束。配置删除不是对已执行操作的撤销；紧急停止由宿主收紧 Policy 或取消受影响 run。

Runtime.run 现在默认 Compose：统一加入 Agent instruction 和技能索引。RunOutput.messages 保留本次有效上下文；持久化 `conversation_messages` 用于下一次运行，避免重复追加托管提示词。完整回放已组装的 messages 可选 InstructionsMode::Preserve；这只改变提示词组装，不跳过工具装配或授权。Preserve 模式下不会重新注入显式选中技能，需提供已组装上下文。历史中的旧工具结果仍是过去的事实，不授予新 run 能力。

context_manifest 返回激活技能的内容指纹与诊断。恢复会话仍需传当前配置；manifest 不自动恢复服务器或技能。模型读取技能时的完整 ToolCall/ToolResult 会保留在 conversation_messages 中。

Tool::call 保持 Result<Value, ToolError>；新增默认 call_output 包装成功结果。需要结构化错误的实现可覆盖 call_output 返回 ToolOutput，而无需修改已有工具宏。新增 RunRequest/RunOutput 字段可能影响使用结构体字面量的调用方，推荐使用 RunRequest::new。

## 验证

运行 `cargo test`、`cargo test --features skills`、`cargo test --features mcp`、`cargo test --all-features`。MCP 集成测试使用本地 Python 3 fixture（`/usr/bin/python3`），不需要业务凭证或在线模型；覆盖 stdio、HTTP、分页、配置更新、凭证隔离、失败结果和取消。库运行本身不依赖 Python，只有用户配置的 MCP 程序可能有自己的依赖。
