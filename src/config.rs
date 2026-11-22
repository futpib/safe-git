//! Config is inert key/value data. Includes, commands and path redirects are never evaluated.
use crate::{
    repo::Repo,
    trust::{self, Binding},
};
use anyhow::{Context, Result, ensure};
use std::collections::BTreeMap;
pub fn parse(bytes: &[u8]) -> Result<BTreeMap<String, String>> {
    ensure!(bytes.len() <= 8 * 1024 * 1024, "config exceeds limit");
    let mut values = BTreeMap::new();
    let mut section = String::new();
    for raw in std::str::from_utf8(bytes)?.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if line.starts_with('[') {
            let inner = line
                .strip_prefix('[')
                .and_then(|s| s.strip_suffix(']'))
                .context("invalid config section")?
                .trim();
            section = if let Some((name, sub)) = inner.split_once(char::is_whitespace) {
                let sub = value(sub.trim())?;
                format!("{}.{}", name.to_ascii_lowercase(), sub)
            } else {
                inner.to_ascii_lowercase()
            };
        } else {
            let (key, v) = line
                .split_once('=')
                .map(|(k, v)| (k.trim(), v.trim()))
                .unwrap_or((line, "true"));
            ensure!(
                !section.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                "invalid config key"
            );
            values.insert(
                format!("{}.{}", section, key.to_ascii_lowercase()),
                value(v)?,
            );
        }
    }
    Ok(values)
}
fn value(input: &str) -> Result<String> {
    let mut out = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for c in input.chars() {
        if escaped {
            out.push(match c {
                'n' => '\n',
                't' => '\t',
                'b' => '\x08',
                '"' => '"',
                '\\' => '\\',
                _ => anyhow::bail!("invalid config escape"),
            });
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            quoted = !quoted;
        } else if !quoted && (c == '#' || c == ';') {
            break;
        } else {
            out.push(c);
        }
    }
    ensure!(!quoted && !escaped, "unterminated config value");
    Ok(out.trim_end().to_owned())
}
pub fn quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\t', "\\t")
            .replace('\x08', "\\b")
    )
}
pub fn local(repo: &Repo) -> Result<BTreeMap<String, String>> {
    parse(
        &repo
            .read("config", 8 * 1024 * 1024)?
            .context("missing config")?,
    )
}
pub fn get(repo: &Repo, overrides: &[String], key: &str) -> Result<Option<String>> {
    let mut value = None;
    // Only identity fields can be inherited from the trusted host. No include processing.
    if key.starts_with("user.") {
        let path = std::env::var_os("GIT_CONFIG_GLOBAL")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::home_dir().map(|h| h.join(".gitconfig")));
        if let Some(path) = path {
            match std::fs::read(path) {
                Ok(bytes) => value = parse(&bytes)?.get(key).cloned(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    if let Some(v) = local(repo)?.get(key) {
        value = Some(v.clone());
    }
    for entry in overrides {
        let (k, v) = entry.split_once('=').context("-c requires name=value")?;
        if k == key {
            value = Some(v.to_owned());
        }
    }
    Ok(value)
}
pub fn set(repo: &Repo, key: &str, new: &str, directory: &std::path::Path) -> Result<()> {
    // The caller may store data, but nothing stored here enables an execution facility.
    let (prefix, name) = key
        .rsplit_once('.')
        .context("config key requires a section")?;
    ensure!(
        matches!(key, "user.name" | "user.email")
            || (prefix.starts_with("remote.") && name == "url"),
        "config writes support identities and explicit remote URL registration only"
    );
    ensure!(
        name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "invalid config key"
    );
    ensure!(
        prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b)),
        "invalid config section"
    );
    let section = if let Some((s, sub)) = prefix.split_once('.') {
        format!("{s} {}", quote(sub))
    } else {
        prefix.to_owned()
    };
    let mut bytes = repo
        .read("config", 8 * 1024 * 1024)?
        .context("missing config")?;
    bytes.extend_from_slice(format!("\n[{section}]\n\t{name} = {}\n", quote(new)).as_bytes());
    ensure!(bytes.len() <= 8 * 1024 * 1024, "config exceeds limit");
    if prefix.starts_with("remote.") && name == "url" {
        let remote = prefix.strip_prefix("remote.").unwrap();
        let branch = repo
            .head()?
            .strip_prefix("refs/heads/")
            .unwrap_or("master")
            .to_owned();
        crate::repo::tracking(remote, &branch)?;
        let mut binding = trust::get(repo)?.unwrap_or(Binding {
            url: String::new(),
            remote: remote.into(),
            branch,
            single: false,
            mirror: false,
            bare: repo.worktree.is_none(),
        });
        ensure!(
            binding.remote == remote,
            "only the registered remote is supported"
        );
        binding.url = crate::network::normalize_from(new, directory)?;
        trust::save(repo, &binding)?;
    }
    repo.atomic("config", &bytes)
}
