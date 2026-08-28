//! User-level configuration.
//!
//! Repository config lives in `.fkit/config`; this is the layer beneath it, so
//! settings like your name are stated once rather than in every repository you
//! ever create.
//!
//! ```text
//!   environment            highest — for one command or one CI job
//!   .fkit/config           this repository
//!   ~/.config/fkit/config  you, everywhere
//! ```
//!
//! The file is plain `key = value`, the same format as the repository config,
//! because two config syntaxes in one tool is one too many.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// Where the user-level config lives.
///
/// Honours `FKIT_CONFIG` (a full path) and `XDG_CONFIG_HOME` before falling
/// back to `~/.config/fkit/config`.
pub fn global_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("FKIT_CONFIG") {
        return Some(PathBuf::from(explicit));
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("fkit").join("config"));
    }
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config").join("fkit").join("config"))
}

/// Parse `key = value` lines, ignoring blanks and `#` comments.
pub fn parse(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

pub fn global_get(key: &str) -> Option<String> {
    let path = global_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    parse(&text, key)
}

pub fn global_set(key: &str, value: &str) -> Result<PathBuf> {
    let path = global_path().context("cannot locate a home directory for the config file")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }

    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut out = String::new();
    let mut replaced = false;
    for line in existing.lines() {
        let is_key = !line.trim_start().starts_with('#')
            && line.split_once('=').map(|(k, _)| k.trim() == key).unwrap_or(false);
        if is_key {
            if !replaced {
                out.push_str(&format!("{key} = {value}\n"));
                replaced = true;
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        out.push_str(&format!("{key} = {value}\n"));
    }

    std::fs::write(&path, out).with_context(|| format!("writing {}", path.display()))?;
    restrict(&path);
    Ok(path)
}

/// The file can hold an access token, so keep it owner-readable only.
#[cfg(unix)]
fn restrict(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict(_path: &std::path::Path) {}

/// Every key currently set, for `fkit config --list`.
pub fn global_all() -> Vec<(String, String)> {
    let Some(path) = global_path() else { return vec![] };
    let Ok(text) = std::fs::read_to_string(path) else { return vec![] };
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            l.split_once('=').map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_values_and_skips_comments() {
        let t = "# a comment\nauthor = Travis <t@e.com>\n\nremote=ws://x/y\n";
        assert_eq!(parse(t, "author").as_deref(), Some("Travis <t@e.com>"));
        assert_eq!(parse(t, "remote").as_deref(), Some("ws://x/y"));
        assert_eq!(parse(t, "missing"), None);
    }

    #[test]
    fn a_commented_out_key_is_not_a_value() {
        assert_eq!(parse("# author = ghost\n", "author"), None);
    }

    #[test]
    fn set_replaces_in_place_and_keeps_other_keys() {
        let dir = std::env::temp_dir().join(format!("fkit-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config");
        unsafe { std::env::set_var("FKIT_CONFIG", &path) };

        global_set("author", "One").unwrap();
        global_set("token", "secret").unwrap();
        global_set("author", "Two").unwrap();

        assert_eq!(global_get("author").as_deref(), Some("Two"));
        assert_eq!(global_get("token").as_deref(), Some("secret"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("author").count(), 1, "must replace, not append");

        unsafe { std::env::remove_var("FKIT_CONFIG") };
        let _ = std::fs::remove_dir_all(dir);
    }
}
