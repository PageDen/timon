// Adapted for Timon. Not derived from Prodex source.
//! An MCP server over stdio, so a lead can delegate.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::mcp::handoff::{self, HandoffPolicy, HandoffRequest};
use crate::mcp::protocol::{
    INTERNAL_ERROR, INVALID_PARAMS, MAX_REQUEST_BYTES, METHOD_NOT_FOUND, PROTOCOL_VERSION, Request,
    Response, tool_result,
};
use crate::mcp::tools::{self, DelegateRequest, WorkerPolicy};

/// What this server offers.
///
/// Two audiences, deliberately separate. A lead's harness gets the delegation
/// tool and nothing else; a developer's Codex session gets the hand-off tool and
/// nothing else. Offering both to either would let a model reach past what its
/// caller meant to give it.
pub enum Offering {
    /// For a lead's harness: delegate one bounded worker.
    Delegation(Box<WorkerPolicy>),
    /// For a developer's session: hand a goal to Timon (P1.2).
    Handoff(HandoffPolicy),
}

/// Serves until stdin closes, which is how the caller says it is finished.
pub async fn serve_offering(offering: Offering) -> anyhow::Result<()> {
    match offering {
        Offering::Delegation(policy) => serve(*policy).await,
        Offering::Handoff(policy) => serve_handoff(policy).await,
    }
}

/// Serves the hand-off tool over stdio.
pub async fn serve_handoff(policy: HandoffPolicy) -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            return Ok(());
        }
        if read > MAX_REQUEST_BYTES {
            eprintln!("timon-handoff: a request over {MAX_REQUEST_BYTES} bytes was dropped");
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(trimmed) {
            Ok(request) => request,
            Err(error) => {
                eprintln!("timon-handoff: unparseable request: {error}");
                continue;
            }
        };
        let Some(id) = request.id.clone() else {
            continue;
        };
        let response = handle_handoff(&policy, &request.method, request.params, id);
        let mut encoded = serde_json::to_string(&response)?;
        encoded.push('\n');
        stdout.write_all(encoded.as_bytes()).await?;
        stdout.flush().await?;
    }
}

fn handle_handoff(policy: &HandoffPolicy, method: &str, params: Value, id: Value) -> Response {
    match method {
        "initialize" => Response::ok(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "timon", "version": env!("CARGO_PKG_VERSION") }
            }),
        ),
        "tools/list" => Response::ok(id, json!({ "tools": [handoff::describe()] })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if name != "timon_handoff" {
                return Response::failed(id, METHOD_NOT_FOUND, format!("no tool named {name:?}"));
            }
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            let request: HandoffRequest = match serde_json::from_value(arguments) {
                Ok(request) => request,
                Err(error) => {
                    // A tool error, not a protocol error: the session should see
                    // what was wrong with the call and be able to correct it.
                    return Response::ok(
                        id,
                        tool_result(format!("The call was not usable: {error}"), true),
                    );
                }
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_secs() as i64)
                .unwrap_or_default();
            let (text, is_error) = handoff::handoff(policy, request, now);
            Response::ok(id, tool_result(text, is_error))
        }
        "ping" => Response::ok(id, json!({})),
        other => Response::failed(
            id,
            METHOD_NOT_FOUND,
            format!("unsupported method {other:?}"),
        ),
    }
}

/// Serves until stdin closes, which is how the harness says it is finished.
pub async fn serve(policy: WorkerPolicy) -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();
    let delegations = AtomicU64::new(0);

    loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            return Ok(());
        }
        if read > MAX_REQUEST_BYTES {
            // Nothing to reply to: without a parsed id there is no id to answer.
            eprintln!("timon-mcp: a request over {MAX_REQUEST_BYTES} bytes was dropped");
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let request: Request = match serde_json::from_str(trimmed) {
            Ok(request) => request,
            Err(error) => {
                eprintln!("timon-mcp: unparseable request: {error}");
                continue;
            }
        };
        // A notification has no id and must not be answered.
        let Some(id) = request.id.clone() else {
            continue;
        };

        let response = handle(&policy, &delegations, &request.method, request.params, id).await;
        let mut encoded = serde_json::to_string(&response)?;
        encoded.push('\n');
        stdout.write_all(encoded.as_bytes()).await?;
        stdout.flush().await?;
    }
}

async fn handle(
    policy: &WorkerPolicy,
    delegations: &AtomicU64,
    method: &str,
    params: Value,
    id: Value,
) -> Response {
    match method {
        "initialize" => Response::ok(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "timon", "version": env!("CARGO_PKG_VERSION") }
            }),
        ),
        "tools/list" => Response::ok(id, json!({ "tools": tools::all() })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if name != "delegate_worker" {
                return Response::failed(id, METHOD_NOT_FOUND, format!("no tool named {name:?}"));
            }
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            let request: DelegateRequest = match serde_json::from_value(arguments) {
                Ok(request) => request,
                Err(error) => {
                    // Reported as a tool error rather than a protocol error, so
                    // the lead sees what it got wrong and can correct it.
                    return Response::ok(
                        id,
                        tool_result(format!("The call was not usable: {error}"), true),
                    );
                }
            };
            let number = delegations.fetch_add(1, Ordering::SeqCst) + 1;
            let (text, is_error) = tools::delegate(policy, number, request).await;
            Response::ok(id, tool_result(text, is_error))
        }
        "ping" => Response::ok(id, json!({})),
        other => Response::failed(
            id,
            METHOD_NOT_FOUND,
            format!("unsupported method {other:?}"),
        ),
    }
}

/// Reports a parameter problem, kept for callers that need the code.
pub fn invalid_params(id: Value, message: impl Into<String>) -> Response {
    Response::failed(id, INVALID_PARAMS, message)
}

/// Reports a server-side failure.
pub fn internal_error(id: Value, message: impl Into<String>) -> Response {
    Response::failed(id, INTERNAL_ERROR, message)
}
