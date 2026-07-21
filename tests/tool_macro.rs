use g::{Tool, ToolCallError, ToolContext, tool};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[tool(name = "lookup", description = "Look up recent values")]
async fn lookup(days: Option<u32>, market: String) -> Result<Value, ToolCallError> {
    Ok(serde_json::to_value(json!({
        "days": days,
        "market": market
    }))?)
}

#[tokio::test]
async fn generates_metadata_schema_and_argument_conversion() {
    let spec = lookup.spec();
    assert_eq!(spec.name, "lookup");
    assert_eq!(spec.description, "Look up recent values");
    assert!(spec.input_schema["properties"]["days"].is_object());
    assert!(spec.input_schema["properties"]["market"].is_object());
    assert_eq!(spec.input_schema["required"], json!(["market"]));

    let result = lookup
        .call(
            ToolContext {
                run_id: Uuid::new_v4(),
                cancellation_token: CancellationToken::new(),
            },
            json!({ "market": "tokyo" }),
        )
        .await
        .unwrap();

    assert_eq!(result, json!({ "days": null, "market": "tokyo" }));
}

#[tokio::test]
async fn reports_missing_and_invalid_arguments() {
    let context = || ToolContext {
        run_id: Uuid::new_v4(),
        cancellation_token: CancellationToken::new(),
    };

    let missing = lookup.call(context(), json!({})).await.unwrap_err();
    assert!(
        missing
            .message
            .contains("missing required argument `market`")
    );

    let invalid = lookup
        .call(context(), json!({ "days": "many", "market": "tokyo" }))
        .await
        .unwrap_err();
    assert!(invalid.message.contains("invalid argument `days`"));
}
