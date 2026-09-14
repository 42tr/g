# g 的 MCP 与 Skills 机制设计

状态：架构设计与后续演进参考。首版实现的实际 API、范围及限制见 [使用说明](extensions.md)，以下示意 API 不代表全部已经落地。已按“每次调用显式传配置、不扫描目录、支持运行时更新”修订。基于 `g@7956f83`，2026-09-14。文中新增 Rust API、配置字段和默认值均为提议，不是已有功能。

## 1. 目标与核心决策

让 g 的 Agent 能使用外部 MCP 工具，并按需读取标准技能包，保留现有 Model、Tool、Policy、EventSink 的主结构。

- **MCP 是工具接入层**：发现远端工具，将其包装为 g::Tool，负责连接、请求、结果和取消。
- **Skills 是知识加载层**：接收调用方明确提供的技能，给模型提供索引，通过工具按需读取说明与资源。
- **两者在每次 run 的装配阶段结合**：技能可以描述如何使用某个工具，但不能凭文档创建连接、安装程序或授予权限。
- 第一版采用同 crate 内的可选 `mcp`、`skills` feature；不建立通用插件框架，不把业务身份、SQLite 和 Hermes 目录规则写死在 g 中。
- 使用官方 rmcp 实现 MCP 协议及传输适配，g 只做运行时桥接。官方 SDK 已提供 child-process 与 Streamable HTTP client 能力。[rmcp 官方仓库](https://github.com/modelcontextprotocol/rust-sdk)

## 2. 模块结构

```text
src/
  agent.rs                  # 模型、本地工具、Policy；不保存动态扩展配置
  request.rs                # 每次调用携带完整 ExtensionConfig（文件名待实现确定）
  runtime.rs                # 创建 RunScope，prepare_run，再进入模型循环
  tool.rs                   # ToolOutput、ToolOrigin、调用上下文
  policy.rs                 # 所有工具和 handoff 统一授权
  event.rs                  # 运行关联、工具与技能事件
  run_scope.rs              # 每 run 的取消、deadline、身份引用、资源租约
  mcp/
    config.rs               # 类型化 server/transport/tool filter 配置
    manager.rs              # 客户端连接、凭证分区、租约与关闭
    discovery.rs            # 分页工具发现、版本化目录快照
    tool.rs                 # McpTool: Tool
    names.rs                # 模型工具别名和原始工具名的双向映射
    result.rs               # MCP result → ToolOutput
  skills/
    config.rs               # 显式技能正文、资源映射和预算
    catalog.rs              # 从配置构建内存索引，不扫描目录
    snapshot.rs             # 每 run 不可变内容和指纹
    prompt.rs               # 技能索引与统一运行提示词
    tools.rs                # skills_list / skills_read
```

Cargo 依赖按 feature 启用：`mcp` 引入 rmcp client/所需 transport；`skills` 引入 YAML 解析及内容摘要实现。锁定具体版本时验证当前 reqwest 0.13 与 SDK 依赖组合，内部不向用户公开 rmcp transport 的具体类型。

第一版只支持 MCP **stdio 与 Streamable HTTP**；Streamable HTTP 的响应可以包含 SSE，这不等同于旧的独立 HTTP+SSE transport。协议协商由 SDK 完成，兼容测试固定已验证的协议版本，避免宣称自动支持所有版本。[MCP 传输规范](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)

## 3. 配置传入与初始化

### 3.1 配置属于每次调用

MCP 与 skills 均由调用方在 RunRequest 中显式传入。Agent 保存模型、本地工具和 Policy；Runtime 持有可复用的 MCP 连接管理器，但不保存业务的“当前配置”。g 不扫描目录、不读取配置文件、不监听文件变更，也不维护全局技能注册表。

以下均为拟议 API，不是已有可编译接口：

```rust
// 宿主创建一次；manager 只持有连接池预算与凭证解析器。
let mcp = Arc::new(McpManager::new(pool_options, credential_provider)?);
let runtime = Runtime::new().with_mcp_manager(mcp.clone());
let agent = Agent::new(model).with_policy(business_policy);

// 每次调用从宿主当前状态构造一份完整配置。
let extensions = Arc::new(ExtensionConfig::try_new(
    mcp_servers,
    skill_definitions,
    extension_limits,
)?);
let request = RunRequest::new(history)
    .with_extensions(extensions)
    .with_scope(invocation_scope)
    .with_selected_skills(["task-analysis"])
    .with_cancellation_token(cancel);
let output = runtime.run(&agent, request).await?;

// 宿主退出时，包含上述 run 失败的路径，也要执行关闭。
mcp.close().await?;
```

每份 ExtensionConfig 是完整快照，不是对上次配置的增量合并。缺省 extensions 或传入空列表都表示本次不启用这些扩展，不能继承上次请求的服务器、技能或凭证。便利运行入口也应支持传入同一 RunRequest，纯文本的旧入口保持空扩展语义。

两个 feature 默认关闭，可独立启用。仅 skills 不需要 McpManager；存在 MCP 配置却没有 manager 时返回配置错误。不创建内部 Tokio runtime，不接管宿主信号处理。

### 3.2 配置数据形态

```rust
// 示意结构；传输细节、预算和可选字段省略。
pub struct ExtensionConfig {
    pub mcp: Vec<McpServerConfig>,
    pub skills: Vec<SkillDefinition>,
    pub limits: ExtensionLimits,
}

pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub resources: BTreeMap<String, SkillResource>,
    pub required_tools: Vec<ToolOrigin>,
    // license、compatibility、metadata 等可选元数据。
}

pub struct SkillResource {
    pub media_type: String,
    pub content: Vec<u8>,
}
```

示例序列化配置由宿主解析后传入，g 不自行找文件：

```json
{
  "mcp": [{
    "id": "task-center",
    "transport": {"type": "streamable_http", "url": "https://task-center.example/mcp"},
    "credential_ref": "current-business-user",
    "required": true,
    "allowed_tools": ["get_instance", "search_instances"],
    "max_in_flight": 4,
    "connect_timeout_ms": 10000,
    "call_timeout_ms": 30000
  }],
  "skills": [{
    "name": "task-analysis",
    "description": "分析任务实例状态、证据和风险",
    "instructions": "先获取任务状态，再根据 references/task-state.md 组织报告。",
    "resources": {
      "references/task-state.md": {"media_type": "text/markdown", "content": "任务状态字段说明……"}
    },
    "required_tools": [{"kind": "mcp", "server_id": "task-center", "tool_name": "get_instance"}]
  }]
}
```

上例文本 content 的序列化形式由 DTO 转成 UTF-8 bytes；不是对 Vec<u8> 默认 serde 表示的承诺。首版技能正文和资源直接随配置提供；资源键是虚拟名称，不是可供 g 打开的文件路径。宿主可从数据库、文件或其他来源构造配置，但这些读取操作不在 g 的初始化机制内。

stdio 配置显式提供 command、args、cwd 和环境集合，command/cwd 使用宿主解析的绝对路径；g 仅按配置启动 MCP 程序，不搜索技能目录。HTTP 凭证通过 credential_ref 与 InvocationScope 交给 provider 解析，配置不依赖模型生成的身份参数。

工具过滤采用显式语义：allowed_tools=[] 表示不暴露工具，全部暴露使用独立 allow_all_tools=true。重复 server ID、重复 skill name、无效资源键、超过预算均拒绝整份配置；宿主如需多来源覆盖，必须在传入前完成合并。

### 3.3 默认初始化：校验快照后按需连接

`ExtensionConfig::try_new` 只做内存校验与内容指纹计算，不产生外部 I/O；实际类型用私有字段与只读访问器保证构造后不可变。上面的 pub 字段仅用于说明数据形态。每次 prepare_run 再检查身份和当前 Policy，不把已校验配置等同于授权。

准备流程：校验请求 → 取得本次技能快照 → 按本次 MCP 配置和用户身份获取连接 → 握手和 tools/list → 过滤工具与校验技能依赖 → 发布 PreparedRun → 调用模型。整个准备过程计入 run deadline。

Skills 无需独立 init、扫描或 refresh；将传入的内容建立索引即可，正文仅在显式选择或 skills_read 时进入模型上下文。MCP 连接按配置指纹和凭证分区缓存，相同 key 的并发冷启动合并为一次初始化。

共享连接初始化由 manager 持有，有独立的有界超时；取消某个等待者只停止其等待，不取消其他等待者需要的连接。无人等待且没有预热需求时可取消并清理。失败条目允许有界退避后重试，不能永久缓存失败。

### 3.4 可选预热

```rust
// 拟议 API：必须传入与计划运行相同的配置和身份。
let report = runtime.warmup(
    &agent,
    WarmupRequest::new(startup_scope)
        .with_extensions(extensions.clone())
        .with_timeout(Duration::from_secs(15))
        .with_cancellation_token(shutdown.child_token()),
).await?;
```

warmup 复用校验、连接、工具发现和技能依赖检查；不调用模型、不调用业务工具、不激活技能正文。WarmupReport 包含配置指纹、server 就绪/降级状态及工具/技能数量，不含凭证。预热不是发布配置，后续 run 仍必须传入 extensions。

没有用户身份时只能对配置做内存校验；需要用户凭证的 MCP 留到真实请求时连接。服务账号可以显式预热自己的分区，但不能被用户请求继承。缓存有 TTL，预热不保证下次连接永远有效。

### 3.5 运行时修改配置

宿主可在服务运行期间随时生成新 ExtensionConfig，并在下一次调用传入；不需要重建 Agent、Runtime 或 McpManager，也不需要向 g 调用全局 update/refresh。若宿主维护共享“当前配置”，由宿主原子替换 Arc，每次调用只读取一次，避免 MCP 和 skills 分别取值导致版本混合。

```rust
// 同一 runtime/agent 可以依次或并发接收不同配置。
let request_v1 = RunRequest::new(history_a).with_extensions(config_v1);
let request_v2 = RunRequest::new(history_b).with_extensions(config_v2);
// v2 可新增/移除服务器，修改 URL、凭证引用、工具过滤、技能正文及资源。
// 提交各自的请求即可；没有修改共享 Agent 的步骤。
```

| 修改 | 新请求 | 已经开始的 run |
| --- | --- | --- |
| 新增/移除 MCP server | 只使用新列表；空列表禁用 MCP | 保留原配置及租约 |
| 修改 URL、command、args、env 或凭证 | 获取新连接分区；旧连接按租约/TTL 回收 | 不把在途请求迁移到新服务器 |
| 修改 allowed_tools 或技能依赖 | 按新配置装配工具和可用技能 | 维持原工具快照，实际调用仍查 Policy |
| 修改/删除技能或资源 | 读取新配置中的内容 | 使用已固定的旧内容，资源不会半途变版本 |
| 紧急撤权 | 当前 Policy 拒绝后续操作 | 宿主同步收紧 Policy 或取消受影响 run |

首版更新在 run 边界生效，不在一次模型循环中热替换已声明的工具和技能。若要让正在进行的会话采用新配置，下一轮 run 传新快照即可；若必须立即切换当前 run，取消后用新配置发起新 run，不自动重放已执行工具。配置删除本身不是对其他请求的全局撤权指令。

g 根据实际配置内容计算指纹，不仅依赖调用方自报 revision。连接 key 含租户/凭证 scope、server ID、连接相关配置指纹、凭证 generation；工具过滤和技能变化不必重建传输，但必须生成新的 PreparedRun。不可观测的服务端变更由工具发现缓存 TTL、listChanged 或显式失效处理，与宿主配置更新区分。

首版 per-server 并发上限放入连接池条目身份，改变上限使用新条目；manager 另设跨配置版本的总容量上限，避免反复更新绕过容量限制。缓存设条目数、字节与 idle TTL 上限。旧配置仍可能被合法请求再次传入，g 不假设 revision 单调；禁止回退应由宿主 Policy 实施。指纹不暴露原始秘密，也不能代替凭证隔离。

### 3.6 失败与生命周期

无效配置在模型调用前失败，不回退到上次请求的配置；required MCP 失败使本次 prepare 失败，optional 失败产生诊断并隐藏其工具，但不能吞掉调用者取消或总超时。显式选择的技能缺失或缺少必需工具时准备失败；其他缺依赖技能标为不可用。

准备失败不发布部分 PreparedRun，释放自身租约；其他请求可复用已经正常建立的池连接。InitError 包含 stage、server/skill 标识、稳定 code、可否重试和脱敏报告。

宿主共享 Arc<McpManager>，不需要共享一个可变 SkillCatalog；技能内容由请求快照 Arc 持有。Agent clone/drop 不关闭池。shutdown 顺序为停止接收请求 → 等待或取消 run → mcp.close().await。close 幂等且有总清理超时，关闭后拒绝新连接；失败启动/运行的路径同样需要关闭。Skills 没有后台任务和文件句柄，最后一个快照引用释放即可。

## 4. 每次运行的装配与隔离

新增内部 `PreparedRun`：不可变工具表、最终 messages、技能快照、MCP 租约，以及每 run 的激活状态。Agent 是可复用模板，不在共享 Agent.tools 上追加远端工具或修改技能状态。

```text
RunRequest（完整 ExtensionConfig）+ Agent
  → 创建 RunScope（run_id、deadline、子 cancellation token、身份引用）
  → 加载被允许的 SkillSnapshot
  → 获取本次配置 server 的 MCP 租约与 ToolSnapshot
  → 合并本地工具、MCP 工具、skills 工具，检查所有名称冲突
  → 统一构造 instruction + 技能索引 + 历史 + 显式技能上下文
  → 模型循环：Policy → Tool → 结果回流
  → 返回 RunOutput / 错误，停止未完成操作，释放租约
```

同一 run 工具 schema 与技能版本固定；更新只影响下一次 run。配额与授权可以在执行前收紧，撤权不必等待下一次 run。已有 tool/model Arc 可以共享，但 run 激活状态、事件 sink 和业务上下文独立。

handoff 子 run 继承父请求的扩展快照，并按子 Agent 的 Policy 进一步收紧；不从共享变量重新读取“最新配置”。请求传入的资源配置是能力候选集合，最终可用范围仍受宿主 Policy 约束。

建议扩展 `ToolContext`：`run_id`、`parent_run_id`、`call_id`、`deadline`、`cancellation_token`、`Arc<InvocationScope>`。InvocationScope 只包含调用方提供的身份/权限引用和非敏感属性；凭证由 CredentialProvider 按引用读取，不能作为模型可填写的工具参数。

## 5. MCP 详细设计

### 5.1 连接和会话

McpManager 管理连接，不为每次 tool call 启动一个新 server。池 key 至少包含 `(server_id, connection_fingerprint, credential_scope_id, credential_generation)`；默认不跨凭证分区复用。有意共享公共只读 server 时由宿主显式使用公共 scope。

连接状态：`Disconnected → Connecting → Ready → Draining → Closed`。并发首次获取同一 key 时合并为一次 connect，失败不留下半初始化连接。初始化期间完成协议/能力协商，只有真正可用的工具才加入快照。

stdio 用程序路径与 args 启动，不经过 shell 字符串拼接；从受控环境集合构建子进程 env，再加入明确的 server 配置和凭证。凭证变更创建新 generation 的进程，旧进程在租约结束后回收，不能按请求改共享进程环境。

Streamable HTTP 的用户授权头绑定到对应凭证分区的客户端。不能把一个用户的 token 写到跨用户共享的默认 headers 中。初版支持宿主提供静态凭证/动态凭证 resolver；需要交互式 OAuth 的 server 返回明确的待认证错误，由宿主完成认证后重试连接，不自动弹浏览器。

不声明也不暗中执行未实现的服务端能力。第一版不提供 sampling、elicitation、roots、prompts/resources 浏览及长任务协议；需要这些能力的 server 必须得到明确的不支持响应。MCP resources 与显式提供的技能资源映射是不同机制。

### 5.2 工具发现与稳定命名

SDK 建立连接后，Discovery 跟随分页游标取得完整 tools/list；对循环游标、总工具数、schema/description 字节设置预算。required server 失败则 prepare 失败；optional server 失败则输出诊断并不暴露其工具。

工具目录保留原始 name、inputSchema、可选 outputSchema 和 annotations，转换为不可变 ToolSnapshot。远端 annotations 只作为提示，不自动授予业务权限，也不据此自动开启并行或重试。工具发现、列表变更通知、执行错误等协议结构以官方规范为依据。[MCP Tools](https://modelcontextprotocol.io/specification/2025-11-25/server/tools)

模型可见名称采用 `mcp__<server>__<tool>`，内部保留 `(server_id, original_tool_name)`，tools/call 始终使用原名。非法字符或超出模型名称预算时规范化并附加原始二元组摘要，碰撞仍报配置错误，不能静默覆盖。初版可用 64 字符作为 g 自身的保守命名预算，后续由 Model 能力声明覆盖。

权限与技能依赖使用稳定 `ToolOrigin::Mcp { server_id, tool_name }`，不依赖截断后的模型别名。工具列表变化只标记缓存失效，下一 run 重新发现；当前 run 不修改发给模型的工具表。远端在当前 run 删除工具时返回 tool_unavailable，不偷偷换同名实现。

schema 原样保留，校验器按明确支持的 JSON Schema 方言编译；不支持的 schema 特性给出诊断或拒绝注册，不悄悄删约束。仅当声明了 outputSchema 时校验相应 structuredContent；provider 对 schema 的额外限制由模型适配器独立报告。

### 5.3 调用、结果和错误

调用顺序：检查取消/剩余 deadline → Policy 授权 → 校验参数 → 申请 server 并发额度 → tools/call → 规范化结果 → 事件与模型回流。等待并发额度也计入 deadline，取消不得消耗真实工具调用。

现有 `Tool::call -> Result<Value, ToolError>` 无法同时保留 MCP 的错误标志与原始结果结构。建议进行一次小范围 API 调整：

```rust
pub struct ToolOutput {
    pub value: serde_json::Value,
    pub is_error: bool,
}

// 提议：Tool::call(...) -> Result<ToolOutput, ToolError>
// ToolOutput::json(value) 是普通成功结果。
```

本地工具宏自动包装原函数的 JSON 结果，使用 `#[tool]` 的函数签名保持不变；手写 Tool 实现需要显式包装返回值，作为 v0.1 开发期的接口变更记录。`Content::ToolResult { result, is_error }` 与 ToolCompleted 已有对应字段，可继续使用。

McpTool 的 value 使用统一 envelope：`{server_id, tool_name, content, structured_content}`。服务端 `isError=true` 是带结构的工具执行结果，保留 content 并设置 ToolOutput.is_error；transport/JSON-RPC 错误使用带 code 和 execution_state 的 ToolError，再由 Runtime 转为模型可见的失败结果。[MCP 错误区分](https://modelcontextprotocol.io/specification/2025-11-25/server/tools)

第一版模型可消费的结果限定为文本和结构化 JSON。非文本 MCP 内容必须显式处理：混合结果保留文本/结构部分并报告 unsupported_content，完整内容进入有大小限制的诊断输出；只有不支持内容时返回工具错误。不能把 base64 无限制塞进提示词，不能称为已支持 MCP 全部多模态结果。下一阶段再扩展 ToolOutput/Provider 转换处理图像、音频及资源内容。

重连仅恢复后续请求。提交后的连接断开/超时标记 `execution_state=unknown`，不重放 tools/call。重试仅限确定未提交的连接/发现操作；服务端自报 idempotent 不足以自动重试业务写操作。

### 5.4 取消、关闭和容量

每次工具调用有独立取消 token，尽可能通过 SDK 对应请求发送取消；取消不是远端回滚保证。取消一个 run 不关闭仍被其他 run 使用的连接。

对无法确认已停止的调用，用有限 drain 时间等待；超时将该连接标记不可复用，再关闭 transport。共享连接上其他请求要收到明确错误，不能悄悄重试。stdio 由 manager 拥有进程句柄，close 时终止、等待退出，并按平台处理工具进程树。

manager.close 停止新租约、取消后台发现/通知任务、等待或取消在途请求、回收连接。Drop 只做非阻塞兜底，不能当作正常 shutdown。

配置连接总数、每 server 并发、空闲 TTL、发现总量、单结果和事件队列字节预算。连接复用不要求同时开启单 run 内并行工具调用；第一版沿用当前的顺序工具执行，只让独立 run 并发。

## 6. Skills 详细设计

### 6.1 技能内容格式

运行入口接收结构化 SkillDefinition：name、description、instructions、显式 resources 和 required_tools。可保留 license、compatibility、metadata 等元数据。正文是 Markdown，不要求磁盘存在 SKILL.md 或某个同名目录。

如宿主已有标准 SKILL.md，可显式使用纯内存辅助函数 `SkillDefinition::from_markdown(text, resources)` 解析已提供的 YAML frontmatter 与正文。它不接收目录、不读取文件，也不自动寻找 references/scripts/assets。格式兼容以 [Agent Skills 规范](https://agentskills.io/specification) 为依据；目录组织由提供文件的宿主处理。

required_tools 是结构化 ToolOrigin 列表，用于检测能力是否齐备，不能连接未配置服务器或扩大权限。实验性的 allowed-tools 元数据可保留，但不赋予预授权效果。技能不能依靠正文自行安装程序或注册工具。

### 6.2 校验与版本

构造时校验名称、描述、正文大小、资源总量、重复键及依赖结构；错误拒绝整份配置，不静默排除调用方提供的技能。无自动发现、目录优先级和 first_root_wins；合并责任属于宿主。

每个 SkillSnapshot 保存不可变内容和指纹，resources 按显式键精确查找。正文和所有资源属于同一快照，旧 run 持有旧 Arc，新 run 可传入新内容；不检查磁盘 mtime，也不依赖 watcher。资源较大时可由宿主复用相同不可变 Arc 内容以减少内存复制。

首版不提供回调读取任意路径或 URL 的资源接口。若以后需要按需从外部存储取资源，应另行设计带版本约束的 resolver，不能破坏当前快照一致性。

### 6.3 渐进加载与工具

只在初始模型上下文暴露索引，任务匹配后再读取正文，需要时继续读取资源，遵循技能的渐进加载方式。[Agent Skills 接入指南](https://agentskills.io/client-implementation/adding-skills-support)

注册两个普通 g 工具：

| 工具 | 输入 | 输出 |
| --- | --- | --- |
| skills_list | query?、cursor?、limit? | 当前已授权 snapshot 的技能元数据、revision、可用性与分页游标 |
| skills_read | name、path?、offset?、limit? | 默认返回配置中的 instructions；path 是 resources 虚拟键，返回文本、revision、截断/分页信息 |

skills_read(name) 成功激活技能，记录本 run 的 active skills 并发 SkillActivated 事件。再读正文时复用缓存，仍可返回正文以支持上下文恢复；累计加载预算按实际送入模型的字节计算，不把“已经激活”当作绕过限制的理由。请求次数仍计入当前 g 的 max_tool_calls。

引用文件可以是 references、assets 或 scripts 中的文本，但读取脚本不会执行它。第一版没有 skills_exec 工具；执行脚本需要宿主另行注册受控终端/进程工具，照常经过 Policy。二进制资源初版只返回类型、大小和不支持说明。

索引先按宿主允许集合过滤，然后受 entry/byte 预算限制，排序稳定；超出部分由 skills_list 查询。优先列出显式选择的技能，再列其余条目。不在初版引入 embedding 或自动技能分类器。

### 6.4 提示词与历史语义

统一 `prepare_run` 为 Agent.run、stream_run 和 Runtime.run 装配提示词，修正当前底层 Runtime.run 不自动带 instruction 的差异。

由运行时维护的 system 区包含 Agent instruction、技能索引和使用 skills_read 的规则；技能正文保持带来源标记的上下文，不能因为被读取就成为新的系统权限。模型主动读取时正文以正常 ToolResult 返回；宿主显式选择技能时，把有来源标记的正文附加到本轮用户上下文，不伪造没有 tool call 的 tool 消息。

保留 RunRequest.messages 作为调用方提供的历史。新增 RunRequest.instructions_mode：默认 Compose，另有 Preserve 用于调用方已经完整构造 system 的场景。内部把“原始历史”和“本 run 生成的系统区”分开存储，避免把输出历史再次传入时重复追加索引和 instruction。

保留 RunOutput.messages 为本次有效上下文，以兼容现有输出语义；新增 `conversation_messages` 专供持久化和续聊，以及 `context_manifest` 记录配置指纹、技能 name/内容指纹、激活记录、有效工具目录版本。conversation_messages 保留调用方原有 system 消息和完整工具调用/结果对，只排除本次由运行时生成且有内部身份的临时提示词片段，不能按相同文本盲删历史。

下一 run 将 conversation_messages 作为历史，采用 Compose 重新装配当前 instruction 和目录；需要完整回放旧 messages 的调用方显式使用 Preserve，避免重复装配。迁移文档与 examples 必须明确两种字段的用途，不让调用者靠字符串去重。Preserve 只影响提示词装配，不跳过工具发现、授权及资源边界检查。

同一会话下一 run 不默认继承上次激活的权限。若提供上次 manifest，先核对本次配置的技能 name/内容指纹和当前允许集合，再按预算恢复；技能变更或撤权时重新加载/明确告知。manifest 不是资源配置，不能据此恢复本次未传入的技能或服务器。g 暂无 compaction，未来压缩必须把 active-skill manifest 纳入恢复协议。

### 6.5 资源访问边界

skills_read 的 path 是 resources 的虚拟键，只能读取本次配置已提供且 Policy 允许的内容。拒绝绝对路径、含 `..` 的路径段、NUL 和歧义分隔符；未知键返回 resource_not_found，不尝试文件系统或网络回退。虚拟 references/task-state.md 不代表本地文件。

限制技能数量、单正文/资源字节、配置总字节以及每 run 累计送入模型的字节；分页游标绑定当前快照指纹。二进制资源第一版只返回类型、大小与不支持说明。读取脚本内容不执行脚本；宿主执行工具仍单独受 Policy 控制。日志不输出凭证或未经授权的技能内容。

## 7. g 核心需要的最小修改

| 位置 | 修改 | 原因 |
| --- | --- | --- |
| Agent / RunRequest | 请求携带 ExtensionConfig；builder 名称冲突记录并在 validate 报错 | 当前 tool()/tools() 会静默覆盖重名工具 |
| runtime.rs | RunScope + prepare_run + 私有运行工具表 | 每 run 资产/凭证隔离，三种入口使用同一装配逻辑 |
| tool.rs | ToolOutput、ToolOrigin、扩展 ToolContext | 保留 MCP 错误结果、稳定授权和调用关联 |
| g-macros | 包装普通成功值为 ToolOutput | 保持宏声明的业务函数写法 |
| policy.rs/runtime.rs | handoff 也先授权，子 run 权限只收紧 | 修复当前 handoff 绕过父 Policy 的路径 |
| event.rs | 所有事件关联 run_id/parent_run_id；加入 ToolStarted.arguments 与目录/技能事件 | MCP/技能调用、多席位运行可追踪 |
| agent.rs/runtime.rs | 取消可控的 RunHandle/StreamHandle，完成时显式清理 | 不能依赖下一次事件发送才发现 receiver 已丢弃 |
| providers/openai_chat.rs | 流完成态与错误 envelope 校验 | 不把截断回答当成功，避免 MCP 工具后状态不确定 |

Policy 规则：可见工具来自配置选择与发现结果，实际调用仍由 Policy 校验。child Policy 与继承的调用边界取交集，父方拒绝的能力不能因 handoff 重新变成 AllowAll。父策略需要显式涵盖委派所需工具；不要把加载某个 skill 当作授权子 Agent 的依据。

创建子 cancellation token，不在超时处理里取消调用方可能共享的 token。run 超时/取消时先取消其内部 token，再等待有界清理；Tool 的后台任务、进程和租约遵循同一生命周期。

新事件建议使用 `EventEnvelope { run_id, parent_run_id, sequence, event }`；sequence 每个 run 单调递增。保留现有 EventSink 的适配入口或做明确版本升级，不能静默改变已有事件消费者的序列化结构。

## 8. 两者协同的完整例子

```text
用户：分析任务 42 的进度和风险
  → prepare_run：task-center 已连接，允许 get_instance/search_instances
  → 模型看到 task-analysis 的名称与用途
  → 模型调用 skills_read({name: "task-analysis"})
  → Policy 允许读取该技能；返回方法与 references 索引
  → 模型按需读取 references/task-state.md
  → 模型调用 mcp__task-center__get_instance({id: 42})
  → Policy 校验任务访问范围，McpTool 发出 tools/call
  → MCP 结果带 is_error 与结构化数据回到模型
  → 模型给出有依据的分析，RunOutput 返回对话与技能版本记录
```

如果没有 get_instance 授权，skills_read 不会把它注册回来；模型只能解释能力不足或使用已授权的替代工具。技能目录与 MCP server 不必一一对应，一个技能可描述多个工具，一个工具也可被多个技能使用。

## 9. 实施顺序与验收

| 阶段 | 内容 | 必须通过的测试 |
| --- | --- | --- |
| P0 核心准备 | ToolOutput、名称冲突、RunScope/Handle、handoff 授权、模型终态 | 现有测试；父/子授权；取消无事件模型；截断流失败；有结构工具错误不丢失 |
| P1 MCP 首版 | rmcp stdio/HTTP、凭证分区、发现、调用、关闭 | 本地 mock MCP server 的完整初始化/分页/调用；两种传输；错误、取消、关闭；不同用户 token 不串用 |
| P2 Skills 首版 | catalog、快照、索引、list/read、统一提示词 | 显式内容校验/分页/预算；无文件访问；未知资源键；显式与模型激活；多 run 状态隔离；重复续聊无索引膨胀 |
| P3 联合场景 | 技能读取后调用 MCP；更新与撤权 | 真实 JSON-RPC 的工具链；新 run 看新版本、旧 run 不混版本；撤权不被 skill/handoff 绕过 |
| P4 后续能力 | 多模态结果、OAuth 引导、更大工具目录、受控脚本 | 按实际需求独立设计并验证，不作为首版默认承诺 |

连接恢复测试必须区分“提交前失败”和“工具已执行、响应丢失”，后者断言不自动重放。并发测试需实际让多个 run 同时进入工具，分别记录身份、参数、结果、取消和资源回收。

feature 矩阵：无 feature、仅 skills、仅 mcp、两者同时启用，均运行 cargo check/test；核心依赖不因关闭 feature 仍强制编译整个 MCP 客户端栈。增加 `examples/mcp_stdio.rs`、`mcp_http.rs`、`skills.rs`、`skills_mcp.rs`，用本地 fixture 完成不依赖真实业务凭证的联合演示。

初始化与更新专项验收：构造配置不发生外部 I/O，技能全流程不访问文件系统；lazy 与 warmup 使用相同配置得到等价结果；并发首次连接只初始化一次；取消首个等待者不影响其他等待者；总 deadline 覆盖准备；空/缺省配置不继承上次能力；同一 Runtime 并发运行不同 URL/凭证/技能配置不串用；相同自报 revision 但不同内容不命中旧快照；更新不要求重建 manager；过滤变化不泄漏工具；旧 run 资源内容固定；无效新配置不回退；多版本连接受总容量限制；重复 close、预热中 close、失败启动后的 close 均回收资源。

现有 `tests/hermes_readiness.rs` 的行为刻画测试在 P0 后应改为目标断言：handoff 必须经过授权、流释放触发及时取消；底层 instruction 用 Compose/Preserve 的显式契约取代当前差异。

首版已经实现按请求传配置、MCP stdio/HTTP、内存 skills、连接分区、核心取消与 handoff 授权。实际实现保持 Tool::call 兼容，通过默认 call_output 方法扩展结果；Warmup 使用 RunRequest。文中的额外事件协议、所有预算可配置化、模型终态增强和后续能力仍属演进设计，具体实现以 extensions.md 为准。
