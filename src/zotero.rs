//! Forwarding MCP requests to Zotero's `/mcp` endpoint on 127.0.0.1.

use serde_json::Value;
use std::time::Duration;

pub const MCP_PATH: &str = "/mcp";
pub const PROTOCOL_VERSION: &str = "2026-07-28";

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("nothing is listening on the Zotero port")]
	NotRunning,
	#[error("Zotero has no MCP endpoint (older Zotero, or MCP is turned off)")]
	NoEndpoint,
	#[error("Zotero didn't answer in time")]
	Timeout,
	#[error("{0}")]
	Other(String),
}

pub struct Client {
	http: reqwest::Client,
	client_id: Option<String>,
}

impl Client {
	pub fn new(client_id: Option<String>, timeout: Duration) -> Self {
		let http = reqwest::Client::builder()
			.timeout(timeout)
			// Never route 127.0.0.1 through an HTTP_PROXY from the environment
			.no_proxy()
			.user_agent(concat!("zotero-mcp-stdio/", env!("CARGO_PKG_VERSION")))
			.build()
			.expect("HTTP client configuration is valid");
		Client { http, client_id }
	}

	pub async fn is_listening(port: u16) -> bool {
		tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok()
	}

	/// Send one JSON-RPC request and return its `result` object.
	pub async fn request(&self, port: u16, method: &str, name: Option<&str>, params: Value) -> Result<Value, Error> {
		let body = serde_json::json!({
			"jsonrpc": "2.0",
			"id": 1,
			"method": method,
			"params": params,
		});
		// reqwest sets Host to 127.0.0.1:<port> and sends no Origin, which
		// Zotero's server requires
		let mut request = self
			.http
			.post(format!("http://127.0.0.1:{port}{MCP_PATH}"))
			.header("Accept", "application/json, text/event-stream")
			.header("MCP-Protocol-Version", PROTOCOL_VERSION)
			.header("Mcp-Method", method)
			.json(&body);
		if let Some(name) = name {
			request = request.header("Mcp-Name", header_safe(name));
		}
		if let Some(id) = &self.client_id {
			request = request.header("Zotero-MCP-Client", header_safe(id));
		}

		let response = request.send().await.map_err(classify)?;
		let status = response.status();
		if status == reqwest::StatusCode::NOT_FOUND {
			return Err(Error::NoEndpoint);
		}
		let bytes = response.bytes().await.map_err(classify)?;
		parse_body(status.as_u16(), &bytes)
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

fn header_safe(value: &str) -> String {
	value.chars().filter(|c| c.is_ascii_graphic() || *c == ' ').collect()
}

fn parse_body(status: u16, body: &[u8]) -> Result<Value, Error> {
	let message: Value = serde_json::from_slice(body).map_err(|_| {
		Error::Other(format!(
			"Zotero returned HTTP {status}: {}",
			String::from_utf8_lossy(body).trim()
		))
	})?;
	if let Some(result) = message.get("result") {
		return Ok(result.clone());
	}
	if let Some(error) = message.get("error") {
		let text = error.get("message").and_then(Value::as_str).unwrap_or("Unknown error");
		return Err(Error::Other(format!("Zotero: {text}")));
	}
	Err(Error::Other(format!(
		"Zotero returned HTTP {status} without a JSON-RPC result"
	)))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_result_and_errors() {
		let ok = br#"{"jsonrpc":"2.0","id":1,"result":{"content":[]}}"#;
		assert_eq!(parse_body(200, ok).unwrap(), serde_json::json!({ "content": [] }));

		let rpc_error = br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"Unknown tool"}}"#;
		match parse_body(400, rpc_error) {
			Err(Error::Other(text)) => assert_eq!(text, "Zotero: Unknown tool"),
			other => panic!("unexpected {other:?}"),
		}

		match parse_body(500, b"Internal error\n") {
			Err(Error::Other(text)) => assert_eq!(text, "Zotero returned HTTP 500: Internal error"),
			other => panic!("unexpected {other:?}"),
		}
	}
}
