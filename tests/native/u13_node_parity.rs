//! U13 cross-runtime proof for the agent-facing MCP contract.
//!
//! This drives the real `lib/mcp.js` through `node -e` and compares its complete JSON values with
//! the Rust protocol engine. The matrix covers built-in methods, the exact 19-tool list, request
//! validation and traversal order, batches, notifications, unknown methods/tools, invalid requests,
//! and the protocol/tool-error split. Set `REQUIRE_NODE_REFERENCE=1` to make an unavailable Node
//! oracle a hard failure rather than a visible skip.

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use artifact_mcp::{
    error::{AppError, JsonRpcError, McpError},
    mcp::{
        dispatch::{ProtocolEra, dispatch_protocol, dispatch_protocol_for_era, validate_tool_call},
        protocol::{OrderedJson, handle_mcp_with, handle_mcp_with_era},
    },
};
use serde_json::{Value, json};

const REQUIRE_NODE_REFERENCE: &str = "REQUIRE_NODE_REFERENCE";

const NODE_DRIVER: &str = r#"
import(process.argv[1]).then(async (mcp) => {
  const requests = JSON.parse(process.argv[2]);
  const auth = { clientId: "publisher", org: "acme", label: "Agent" };
  const outputs = [];
  for (const request of requests) {
    outputs.push(await mcp.handleMcp(request, auth, { notify() {} }));
  }
  process.stdout.write(JSON.stringify(outputs));
}).catch((error) => { console.error(error); process.exit(1); });
"#;

const NODE_MODERN_DRIVER: &str = r#"
import(process.argv[1]).then(async (mcp) => {
  const requests = JSON.parse(process.argv[2]);
  const auth = { clientId: "publisher", org: "acme", label: "Agent" };
  const outputs = [];
  for (const request of requests) {
    outputs.push(await mcp.handleMcp(request, auth, {
      notify() {},
      protocolVersion: "2026-07-28"
    }));
  }
  process.stdout.write(JSON.stringify(outputs));
}).catch((error) => { console.error(error); process.exit(1); });
"#;

const NODE_CONTRACTS_DRIVER: &str = r#"
import(process.argv[1]).then((contracts) => {
  const schema = {
    type: "object",
    properties: {
      html: { type: "string" },
      files: { type: "object", additionalProperties: { type: "string" } }
    },
    required: ["html"],
    additionalProperties: false
  };
  const cases = JSON.parse(process.argv[2]);
  process.stdout.write(JSON.stringify(cases.map((value) => contracts.validateSchemaInput(schema, value))));
}).catch((error) => { console.error(error); process.exit(1); });
"#;

const CONTRACT_CASES: &str = r#"[
  {"files":{"z.css":42,"a.css":false},"surprise":true,"alpha":true},
  {"html":"ok","files":{"10":42,"2":false,"z":0}},
  [],
  {"html":false,"files":null}
]"#;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn require_node_reference() -> bool {
    std::env::var(REQUIRE_NODE_REFERENCE).is_ok_and(|value| value == "1")
}

fn node_reference_available(root: &Path) -> bool {
    let unavailable = if !root.join("lib/mcp.js").is_file() {
        Some("lib/mcp.js is missing")
    } else if !root
        .join("node_modules/better-sqlite3/package.json")
        .is_file()
    {
        Some("node_modules/better-sqlite3 is missing")
    } else {
        match Command::new("node").arg("--version").output() {
            Ok(output) if output.status.success() => None,
            _ => Some("node is not on PATH"),
        }
    };

    match unavailable {
        None => true,
        Some(reason) => {
            assert!(
                !require_node_reference(),
                "{REQUIRE_NODE_REFERENCE}=1 but the Node reference is unavailable ({reason}); \
                 the U13 Node/Rust parity proof did not run"
            );
            eprintln!("skipping U13 Node parity proof: {reason}");
            eprintln!("set {REQUIRE_NODE_REFERENCE}=1 to make this a failure instead");
            false
        }
    }
}

fn temp_data_dir() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("artifact-mcp-u13-{}-{nonce}", std::process::id()))
}

fn run_node(root: &Path, requests: &Value) -> Value {
    run_node_with_driver(root, requests, NODE_DRIVER)
}

fn run_node_with_driver(root: &Path, requests: &Value, driver: &str) -> Value {
    let data_dir = temp_data_dir();
    std::fs::create_dir_all(&data_dir).expect("create Node parity data dir");
    let module = format!("file://{}", root.join("lib/mcp.js").display());
    let output = Command::new("node")
        .current_dir(root)
        .arg("-e")
        .arg(driver)
        .arg(module)
        .arg(requests.to_string())
        .env("DATA_DIR", &data_dir)
        .env("PUBLIC_BASE_URL", "http://conformance.test")
        .output()
        .expect("run the Node MCP reference");
    let _ignored = std::fs::remove_dir_all(&data_dir);
    assert!(
        output.status.success(),
        "Node MCP reference failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Node MCP reference emitted JSON")
}

fn run_node_contracts(root: &Path) -> Value {
    let module = format!("file://{}", root.join("lib/contracts.js").display());
    let output = Command::new("node")
        .current_dir(root)
        .arg("-e")
        .arg(NODE_CONTRACTS_DRIVER)
        .arg(module)
        .arg(CONTRACT_CASES)
        .output()
        .expect("run the Node contracts reference");
    assert!(
        output.status.success(),
        "Node contracts reference failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Node contracts reference emitted JSON")
}

async fn parity_dispatch(message: OrderedJson) -> Result<Value, McpError> {
    if let Some(result) = dispatch_protocol(&message) {
        return result;
    }
    let call = validate_tool_call(message.get("params"))?;
    if call.name == "set_visibility" {
        return Err(AppError::NotFound("Unknown artifact: missing123456".to_owned()).into());
    }
    Err(JsonRpcError::Internal(format!(
        "parity driver did not expect to execute {}",
        call.name
    ))
    .into())
}

async fn modern_parity_dispatch(message: OrderedJson) -> Result<Value, McpError> {
    if let Some(result) = dispatch_protocol_for_era(&message, ProtocolEra::Modern) {
        return result;
    }
    let call = validate_tool_call(message.get("params"))?;
    if call.name == "set_visibility" {
        return Err(AppError::NotFound("Unknown artifact: missing123456".to_owned()).into());
    }
    Err(JsonRpcError::Internal(format!(
        "parity driver did not expect to execute {}",
        call.name
    ))
    .into())
}

fn modern_meta(version: &str, capabilities: Value) -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": version,
        "io.modelcontextprotocol/clientInfo": {
            "name": "artifact-mcp-parity",
            "version": "1.0.0"
        },
        "io.modelcontextprotocol/clientCapabilities": capabilities
    })
}

fn modern_requests() -> Value {
    let meta = modern_meta("2026-07-28", json!({}));
    json!([
        {
            "jsonrpc": "2.0",
            "id": "discover",
            "method": "server/discover",
            "params": { "_meta": meta.clone() }
        },
        {
            "jsonrpc": "2.0",
            "id": "tools",
            "method": "tools/list",
            "params": { "_meta": meta.clone() }
        },
        {
            "jsonrpc": "2.0",
            "id": "resource-templates",
            "method": "resources/templates/list",
            "params": { "_meta": meta.clone() }
        },
        {
            "jsonrpc": "2.0",
            "id": "legacy-method",
            "method": "initialize",
            "params": { "_meta": meta.clone() }
        },
        {
            "jsonrpc": "2.0",
            "id": "unknown-tool",
            "method": "tools/call",
            "params": {
                "name": "no_such_tool",
                "arguments": {},
                "_meta": meta.clone()
            }
        },
        {
            "jsonrpc": "2.0",
            "id": "tool-error",
            "method": "tools/call",
            "params": {
                "name": "set_visibility",
                "arguments": { "id": "missing123456", "hidden": true },
                "_meta": meta.clone()
            }
        },
        {
            "jsonrpc": "2.0",
            "id": "unsupported",
            "method": "tools/list",
            "params": {
                "_meta": modern_meta("2099-01-01", json!({}))
            }
        },
        {
            "jsonrpc": "2.0",
            "id": "missing-capabilities",
            "method": "tools/list",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28"
                }
            }
        },
        [
            {
                "jsonrpc": "2.0",
                "id": "batch",
                "method": "tools/list",
                "params": { "_meta": meta }
            }
        ]
    ])
}

fn requests() -> Value {
    json!([
        { "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18" } },
        { "jsonrpc": "2.0", "id": 2, "method": "initialize", "params": { "protocolVersion": "" } },
        { "jsonrpc": "2.0", "id": 3, "method": "ping" },
        { "jsonrpc": "2.0", "method": "notifications/initialized" },
        { "jsonrpc": "2.0", "id": 4, "method": "tools/list" },
        { "jsonrpc": "2.0", "id": 5, "method": "does/not/exist" },
        { "jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": { "name": "no_such_tool", "arguments": {} } },
        { "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": { "name": "publish_artifact", "arguments": { "z": true, "surprise": true } } },
        { "jsonrpc": "2.0", "id": 8, "method": "tools/call", "params": { "name": "publish_artifact", "arguments": { "html": 123 } } },
        { "jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": { "name": "publish_bundle", "arguments": { "files": { "z.html": 42, "a.html": false } } } },
        { "jsonrpc": "2.0", "id": 10, "method": "tools/call", "params": { "name": "publish_artifact", "arguments": [] } },
        { "jsonrpc": "2.0", "id": 11, "method": "tools/call", "params": { "name": "set_visibility", "arguments": { "id": "missing123456", "hidden": true } } },
        [],
        [
          { "jsonrpc": "2.0", "id": 12, "method": "initialize", "params": { "protocolVersion": "2025-06-18" } },
          { "jsonrpc": "2.0", "method": "ping" },
          { "jsonrpc": "2.0", "id": 13, "method": "ping" }
        ],
        { "jsonrpc": "1.0", "id": "bad", "method": "ping" },
        42
    ])
}

#[tokio::test]
async fn protocol_contract_matches_the_real_node_reference() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }
    let requests = requests();
    let node = run_node(&root, &requests);
    let mut rust = Vec::new();
    for request in requests.as_array().expect("request matrix is an array") {
        let ordered: OrderedJson =
            serde_json::from_value(request.clone()).expect("request converts to ordered JSON");
        rust.push(
            handle_mcp_with(ordered, parity_dispatch)
                .await
                .unwrap_or(Value::Null),
        );
    }
    assert_eq!(Value::Array(rust), node);
}

#[tokio::test]
async fn modern_protocol_contract_matches_the_real_node_reference() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }
    let requests = modern_requests();
    let node = run_node_with_driver(&root, &requests, NODE_MODERN_DRIVER);
    let mut rust = Vec::new();
    for request in requests.as_array().expect("request matrix is an array") {
        let ordered: OrderedJson =
            serde_json::from_value(request.clone()).expect("request converts to ordered JSON");
        rust.push(
            handle_mcp_with_era(ordered, ProtocolEra::Modern, modern_parity_dispatch)
                .await
                .unwrap_or(Value::Null),
        );
    }
    assert_eq!(Value::Array(rust), node);
}

#[test]
fn validator_traversal_matches_the_real_node_contracts_module() {
    let root = repo_root();
    if !node_reference_available(&root) {
        return;
    }
    let schema = json!({
        "type": "object",
        "properties": {
            "html": { "type": "string" },
            "files": { "type": "object", "additionalProperties": { "type": "string" } }
        },
        "required": ["html"],
        "additionalProperties": false
    });
    let cases: OrderedJson = serde_json::from_str(CONTRACT_CASES).expect("valid contract cases");
    let rust = cases
        .as_array()
        .expect("contract cases are an array")
        .iter()
        .map(|value| {
            artifact_mcp::mcp::validation::validate_schema_input(&schema, value, &["html", "files"])
        })
        .collect::<Vec<_>>();
    assert_eq!(
        serde_json::to_value(rust).expect("serialize Rust errors"),
        run_node_contracts(&root)
    );
}
