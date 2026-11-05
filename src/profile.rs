//! Locating the Zotero profile whose sidecar we read.
//!
//! The profile root is the directory that holds `profiles.ini`. The default
//! profile is chosen the same way as `Zotero.Profile.getDefaultInProfilesDir()`
//! in chrome/content/zotero/xpcom/profile.js: the section with `Default=1`,
//! otherwise the last section.

use std::path::{Path, PathBuf};

/// The directory containing `profiles.ini` for the current user, if the
/// platform's home or app-data location can be determined. Matches where
/// Gecko's `nsXREDirProvider` puts it for vendor "Zotero", name "Zotero".
pub fn profile_root() -> Option<PathBuf> {
	#[cfg(target_os = "macos")]
	{
		let home = std::env::var_os("HOME")?;
		Some(PathBuf::from(home).join("Library/Application Support/Zotero"))
	}
	#[cfg(target_os = "windows")]
	{
		let appdata = std::env::var_os("APPDATA")?;
		Some(PathBuf::from(appdata).join("Zotero").join("Zotero"))
	}
	#[cfg(not(any(target_os = "macos", target_os = "windows")))]
	{
		let home = PathBuf::from(std::env::var_os("HOME")?);
		Some(linux_profile_root(
			&home,
			std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
			std::env::var_os("MOZ_LEGACY_HOME").is_some_and(|v| v.as_encoded_bytes().first() == Some(&b'1')),
		))
	}
}

/// `~/.zotero/zotero` if it exists or `MOZ_LEGACY_HOME=1` forces it, otherwise
/// `$XDG_CONFIG_HOME/zotero/zotero`. A relative `XDG_CONFIG_HOME` is invalid
/// per the XDG spec and ignored, as Gecko does.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn linux_profile_root(home: &Path, xdg_config_home: Option<PathBuf>, force_legacy: bool) -> PathBuf {
	let legacy = home.join(".zotero/zotero");
	if force_legacy || legacy.is_dir() {
		return legacy;
	}
	let config = xdg_config_home
		.filter(|dir| dir.is_absolute())
		.unwrap_or_else(|| home.join(".config"));
	config.join("zotero/zotero")
}

/// The default profile directory under `root`, or `None` if `profiles.ini`
/// is missing or names no profile.
pub fn default_profile(root: &Path) -> Option<PathBuf> {
	let ini = std::fs::read_to_string(root.join("profiles.ini")).ok()?;
	default_profile_from_ini(root, &ini)
}

fn default_profile_from_ini(root: &Path, ini: &str) -> Option<PathBuf> {
	let mut sections: Vec<Vec<(String, String)>> = Vec::new();
	let mut current: Option<usize> = None;
	let mut default: Option<usize> = None;
	for line in ini.lines() {
		let line = line.trim();
		if line.starts_with('[') && line.ends_with(']') {
			sections.push(Vec::new());
			current = Some(sections.len() - 1);
		} else if let (Some(i), Some((key, value))) = (current, line.split_once('=')) {
			if key == "Default" && value == "1" {
				default = Some(i);
			}
			sections[i].push((key.to_string(), value.to_string()));
		}
	}
	let section = &sections[default.or(current)?];
	let get = |name: &str| section.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
	let path = get("Path")?;
	let dir = if get("IsRelative") == Some("1") {
		path.split('/').fold(root.to_path_buf(), |dir, part| dir.join(part))
	} else {
		PathBuf::from(path)
	};
	dir.is_dir().then_some(dir)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn picks_default_section_over_last() {
		let root = tempfile::tempdir().unwrap();
		let root = root.path();
		std::fs::create_dir_all(root.join("Profiles/abc.default")).unwrap();
		std::fs::create_dir_all(root.join("Profiles/xyz.other")).unwrap();
		let ini = "[General]\nStartWithLastProfile=1\n\n\
			[Profile0]\nName=default\nIsRelative=1\nPath=Profiles/abc.default\nDefault=1\n\n\
			[Profile1]\nName=other\nIsRelative=1\nPath=Profiles/xyz.other\n";
		assert_eq!(
			default_profile_from_ini(root, ini),
			Some(root.join("Profiles").join("abc.default"))
		);

		// Without Default=1, the last section wins
		let ini = ini.replace("Default=1\n", "");
		assert_eq!(
			default_profile_from_ini(root, &ini),
			Some(root.join("Profiles").join("xyz.other"))
		);
	}

	#[test]
	fn missing_profile_dir_is_none() {
		let root = tempfile::tempdir().unwrap();
		let ini = "[Profile0]\nIsRelative=1\nPath=Profiles/gone\nDefault=1\n";
		assert_eq!(default_profile_from_ini(root.path(), ini), None);
	}

	#[cfg(not(any(target_os = "macos", target_os = "windows")))]
	#[test]
	fn linux_root_prefers_existing_legacy_dir_then_xdg() {
		let home = tempfile::tempdir().unwrap();
		let home = home.path();
		let xdg = Some(PathBuf::from("/xdg"));

		assert_eq!(
			linux_profile_root(home, None, false),
			home.join(".config/zotero/zotero")
		);
		assert_eq!(
			linux_profile_root(home, xdg.clone(), false),
			PathBuf::from("/xdg/zotero/zotero")
		);
		// A relative XDG_CONFIG_HOME is ignored
		assert_eq!(
			linux_profile_root(home, Some(PathBuf::from("rel")), false),
			home.join(".config/zotero/zotero")
		);
		assert_eq!(linux_profile_root(home, xdg.clone(), true), home.join(".zotero/zotero"));

		std::fs::create_dir_all(home.join(".zotero/zotero")).unwrap();
		assert_eq!(linux_profile_root(home, xdg, false), home.join(".zotero/zotero"));
	}
}
