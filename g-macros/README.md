# glow-agent-macros

Attribute macros for declaring asynchronous Rust functions as agent tools.
Use the `tool` macro through the runtime's `glow_agent::tool` re-export; applications
only need a dependency on the runtime package.

```rust
use glow_agent::{tool, ToolCallError};

#[tool(name = "add", description = "Add two integers")]
async fn add(left: i64, right: i64) -> Result<i64, ToolCallError> {
    Ok(left + right)
}
```

The macro generates the `Tool` implementation, JSON Schema, argument
deserialization, and output serialization. It supports optional parameters,
nested input types, and an injected `ToolContext` parameter.

The runtime and this macro package are maintained together in
[42tr/g](https://github.com/42tr/g).

Licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE),
at your option.
