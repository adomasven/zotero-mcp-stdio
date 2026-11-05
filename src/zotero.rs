//! Forwarding MCP requests to Zotero's `/mcp` endpoint on 127.0.0.1.

use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::time::Instant;

pub const MCP_PATH: &str = "/mcp";
pub const PROTOCOL_VERSION: &str = "2026-07-28";

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("nothing is listening on the Zotero port")]
	NotRunning,
	#[error("Zotero has no MCP endpoint (older Zotero, or MCP is turned off)")]
	NoEndpoint,
	/// The request was sent, but no answer came before the deadline. Zotero
	/// may still carry it out.
	#[error("Zotero didn't answer in time")]
	Timeout,
	#[error("{0}")]
	Other(String),
}

pub struct Client {
	http: reqwest::Client,
	client_id: Option<String>,
	next_id: AtomicU64,
}

impl Client {
	pub fn new(client_id: Option<String>) -> Self {
		let http = reqwest::Client::builder()
			// Never route 127.0.0.1 through an HTTP_PROXY from the environment
			.no_proxy()
			// Zotero never redirects. Following one would send the request,
			// library data included, to wherever the redirect points.
			.redirect(reqwest::redirect::Policy::none())
			.user_agent(concat!("zotero-mcp-stdio/", env!("CARGO_PKG_VERSION")))
			.build()
			.expect("HTTP client configuration is valid");
		Client {
			http,
			client_id,
			next_id: AtomicU64::new(1),
		}
	}

	/// Whether something accepts connections on `port`, giving up at `deadline`.
	pub async fn is_listening(port: u16, deadline: Instant) -> bool {
		let connect = tokio::net::TcpStream::connect(("127.0.0.1", port));
		matches!(tokio::time::timeout_at(deadline, connect).await, Ok(Ok(_)))
	}

	/// Send one JSON-RPC request and return its `result` object, giving up at
	/// `deadline`. `name` must be a valid header value; callers pass tool
	/// names, which are checked when tools.json is loaded.
	pub async fn request(
		&self,
		port: u16,
		method: &str,
		name: Option<&str>,
		params: Value,
		deadline: Instant,
	) -> Result<Value, Error> {
		let id = self.next_id.fetch_add(1, Ordering::Relaxed);
		let body = serde_json::json!({
			"jsonrpc": "2.0",
			"id": id,
			"method": method,
			"params": params,
		});
		let remaining = deadline.saturating_duration_since(Instant::now());
		if remaining.is_zero() {
			return Err(Error::Timeout);
		}
		// reqwest sets Host to 127.0.0.1:<port> and sends no Origin, which
		// Zotero's server requires
		let mut request = self
			.http
			.post(format!("http://127.0.0.1:{port}{MCP_PATH}"))
			.timeout(remaining)
			.header("Accept", "application/json, text/event-stream")
			.header("MCP-Protocol-Version", PROTOCOL_VERSION)
			.header("Mcp-Method", method)
			.json(&body);
		if let Some(name) = name {
			request = request.header("Mcp-Name", name);
		}
		if let Some(client_id) = &self.client_id {
			request = request.header("Zotero-MCP-Client", client_id);
		}

		let response = request.send().await.map_err(classify)?;
		let status = response.status();
		if status == reqwest::StatusCode::NOT_FOUND {
			return Err(Error::NoEndpoint);
		}
		let bytes = response.bytes().await.map_err(classify)?;
		parse_body(status, id, &bytes)
	}
}

/// A valid `--client` value: what Zotero shows and keys permissions on, so
/// it's checked once instead of being altered to fit in a header.
pub fn parse_client_id(value: &str) -> Result<String, String> {
	let valid = !value.is_empty()
		&& value.len() <= 64
		&& value.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
	if valid {
		Ok(value.to_owned())
	} else {
		Err("use 1-64 letters, digits, '.', '_' or '-'".into())
	}
}

fn classify(e: reqwest::Error) -> Error {
	if e.is_timeout() {
		Error::Timeout
	} else if e.is_connect() {
		Error::NotRunning
	} else {
		Error::Other(e.to_string())
	}
}

/// Check the JSON-RPC response envelope and return `result`.
fn parse_body(status: reqwest::StatusCode, id: u64, body: &[u8]) -> Result<Value, Error> {
	let unexpected = || {
		Error::Other(format!(
			"Zotero returned an unexpected response (HTTP {status}): {}",
			String::from_utf8_lossy(body).trim()
		))
	};
	let Ok(Value::Object(mut message)) = serde_json::from_slice::<Value>(body) else {
		return Err(unexpected());
	};
	if message.get("jsonrpc") != Some(&Value::from("2.0")) || message.get("id") != Some(&Value::from(id)) {
		return Err(unexpected());
	}
	match (message.remove("result"), message.remove("error")) {
		(Some(result), None) if status.is_success() => Ok(result),
		(None, Some(error)) => {
			let text = error.get("message").and_then(Value::as_str).unwrap_or("Unknown error");
			Err(Error::Other(format!("Zotero: {text}")))
		}
		_ => Err(unexpected()),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use reqwest::StatusCode;

	fn parse(status: u16, body: &str) -> Result<Value, Error> {
		parse_body(StatusCode::from_u16(status).unwrap(), 7, body.as_bytes())
	}

	#[test]
	fn accepts_a_result_or_error_for_the_request_sent() {
		let ok = parse(200, r#"{"jsonrpc":"2.0","id":7,"result":{"content":[]}}"#);
		assert_eq!(ok.unwrap(), serde_json::json!({ "content": [] }));

		let rpc_error = parse(
			400,
			r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32602,"message":"Unknown tool"}}"#,
		);
		assert!(matches!(rpc_error, Err(Error::Other(text)) if text == "Zotero: Unknown tool"));
	}

	#[test]
	fn rejects_responses_that_are_not_for_this_request() {
		for (status, body) in [
			(200, r#"{"jsonrpc":"2.0","id":8,"result":{}}"#),
			(200, r#"{"id":7,"result":{}}"#),
			(200, r#"{"jsonrpc":"2.0","id":7,"result":{},"error":{"message":"x"}}"#),
			(200, r#"{"jsonrpc":"2.0","id":7}"#),
			(500, r#"{"jsonrpc":"2.0","id":7,"result":{}}"#),
			(500, "Internal error"),
			(307, ""),
		] {
			assert!(
				matches!(parse(status, body), Err(Error::Other(_))),
				"{status} {body} was accepted"
			);
		}
	}

	#[test]
	fn client_ids_are_checked_not_rewritten() {
		assert_eq!(parse_client_id("claude-desktop").unwrap(), "claude-desktop");
		for bad in ["", "codex\r\nX-Evil: 1", "kódex", "has space", &"a".repeat(65)] {
			assert!(parse_client_id(bad).is_err(), "{bad:?} was accepted");
		}
	}
}
