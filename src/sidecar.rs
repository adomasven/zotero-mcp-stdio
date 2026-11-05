//! The sidecar file Zotero writes into its profile directory on every launch.
//!
//! It tells this server where the running Zotero listens and how to start
//! Zotero when it isn't running. Example:
//!
//! ```json
//! {
//!   "version": 1,
//!   "zoteroVersion": "10.1",
//!   "port": 23119,
//!   "launch": { "command": "/usr/bin/open", "args": ["-a", "/Applications/Zotero.app"] }
//! }
//! ```

use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "zotero-mcp.json";
/// The sidecar format this build understands. Zotero bumps `version` only for
/// changes an older reader would misread; adding fields doesn't need a bump.
pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sidecar {
	pub port: u16,
	pub launch: Option<Launch>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Launch {
	pub command: String,
	#[serde(default)]
	pub args: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("can't read {path}: {source}")]
	Unreadable { path: PathBuf, source: std::io::Error },
	#[error("{FILE_NAME} is invalid: {0}")]
	Invalid(#[from] serde_json::Error),
	#[error("{FILE_NAME} has format {0}, but this zotero-mcp-stdio only reads format {FORMAT_VERSION}")]
	Unsupported(u32),
}

/// The sidecar in `profile_dir`, or `None` if Zotero hasn't written one.
pub fn read(profile_dir: &Path) -> Result<Option<Sidecar>, Error> {
	let path = profile_dir.join(FILE_NAME);
	let text = match std::fs::read_to_string(&path) {
		Ok(text) => text,
		// Only a missing file means Zotero hasn't written one
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(source) => return Err(Error::Unreadable { path, source }),
	};
	// Check the version before the other fields, so a future format that
	// changes them is reported as unsupported rather than as invalid
	#[derive(Deserialize)]
	struct Versioned {
		version: u32,
	}
	let Versioned { version } = serde_json::from_str(&text)?;
	if version != FORMAT_VERSION {
		return Err(Error::Unsupported(version));
	}
	Ok(Some(serde_json::from_str(&text)?))
}
