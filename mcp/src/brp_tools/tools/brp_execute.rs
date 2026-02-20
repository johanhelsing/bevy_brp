//! `brp_execute` allows for executing an arbitrary BRP method - generally this is used as a
//! debugging tool for this MCP server but can also be used to call custom methods registered
//! by the application (e.g., `my_game/spawn_enemy`).
use std::time::Duration;

use async_trait::async_trait;
use bevy_brp_mcp_macros::ParamStruct;
use bevy_brp_mcp_macros::ResultStruct;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::brp_tools::Port;
use crate::error::Error;
use crate::tool::ToolFn;

#[derive(Clone, Deserialize, Serialize, JsonSchema, ParamStruct)]
pub struct ExecuteParams {
    /// The BRP method to execute (e.g., `rpc.discover`, `world.get_components`, `my_game/do_thing`)
    pub method: String,
    /// Optional parameters for the method
    #[to_metadata(skip_if_none)]
    pub params: Option<serde_json::Value>,
    /// The BRP port (default: 15702)
    #[serde(default)]
    pub port:   Port,
}

/// Result type for the dynamic BRP execute tool
#[derive(Serialize, ResultStruct)]
#[brp_result]
pub struct ExecuteResult {
    /// The raw BRP response data
    #[serde(skip_serializing_if = "Option::is_none")]
    #[to_result(skip_if_none)]
    pub result: Option<Value>,

    /// Message template for formatting responses
    #[to_message(message_template = "Executed method {method}")]
    message_template: String,
}

pub struct BrpExecute;

#[async_trait]
impl ToolFn for BrpExecute {
    type Output = ExecuteResult;
    type Params = ExecuteParams;

    async fn handle_impl(&self, params: ExecuteParams) -> crate::error::Result<ExecuteResult> {
        // Build JSON-RPC request directly so we can pass arbitrary method strings
        // (BrpClient requires the typed BrpMethod enum)
        let url = format!("http://127.0.0.1:{}/jsonrpc", params.port);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": params.method,
            "id": 1,
            "params": params.params,
        });

        let response = reqwest::Client::new()
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| {
                Error::tool_call_failed(format!(
                    "HTTP request failed for {} on port {}: {e}",
                    params.method, params.port
                ))
            })?;

        let response_json: Value = response.json().await.map_err(|e| {
            Error::tool_call_failed(format!("Failed to parse BRP response: {e}"))
        })?;

        if let Some(error) = response_json.get("error") {
            let message = error
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("Unknown BRP error");
            Err(Error::tool_call_failed(message).into())
        } else {
            let result = response_json.get("result").cloned();
            Ok(ExecuteResult::new(result))
        }
    }
}
