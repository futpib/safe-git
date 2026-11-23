//! A bounded, inert subset of host SSH configuration. No Include, Match exec,
//! ProxyCommand, environment expansion or command substitution is evaluated.
use crate::transport::{Auth, Remote};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::PathBuf,
};

pub struct Settings {
    pub remote: Remote,
    pub identity: Option<PathBuf>,
    pub agent: bool,
    pub known_hosts: PathBuf,
    pub jump: Option<String>,
}

pub fn pattern(pattern: &str, value: &str) -> bool {
    if pattern.len() > 4096 || value.len() > 4096 {
        return false;
    }
    let mut row = vec![false; value.len() + 1];
    row[0] = true;
    for p in pattern.bytes() {
        let mut next = vec![false; row.len()];
        if p == b'*' {
            next[0] = row[0];
        }
        for (i, b) in value.bytes().enumerate() {
            next[i + 1] = if p == b'*' {
                row[i + 1] || next[i]
            } else {
                row[i] && (p == b'?' || p.eq_ignore_ascii_case(&b))
            };
        }
        row = next;
    }
    row[value.len()]
}
fn matches(patterns: &[String], host: &str) -> bool {
    let mut positive = false;
    for p in patterns {
        if let Some(p) = p.strip_prefix('!') {
            if pattern(p, host) {
                return false;
            }
        } else {
            positive |= pattern(p, host);
        }
    }
    positive
}
fn parse(text: &str, host: &str) -> Result<HashMap<String, String>> {
    let mut active = true;
    let mut values = HashMap::new();
    for line in text.lines() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let pos = line
            .find(|c: char| c.is_whitespace() || c == '=')
            .unwrap_or(line.len());
        let key = line[..pos].to_ascii_lowercase();
        let rest = line[pos..]
            .trim_start()
            .trim_start_matches('=')
            .trim_start();
        let words = shlex::split(rest).context("invalid host SSH configuration quoting")?;
        if key == "host" {
            active = matches(&words, host);
            continue;
        }
        if key == "match" {
            // A conditional block is never evaluated, including exec predicates.
            active = false;
            continue;
        }
        if !active {
            continue;
        }
        if matches!(
            key.as_str(),
            "proxycommand" | "localcommand" | "knownhostscommand"
        ) {
            ensure!(
                words.as_slice() == ["none"],
                "executable SSH configuration is unsupported"
            );
            continue;
        }
        if matches!(
            key.as_str(),
            "hostname"
                | "user"
                | "port"
                | "identityfile"
                | "identitiesonly"
                | "userknownhostsfile"
                | "proxyjump"
        ) {
            ensure!(words.len() == 1, "SSH option requires one value");
            values.entry(key).or_insert_with(|| words[0].clone());
        }
    }
    Ok(values)
}
fn config(host: &str) -> Result<HashMap<String, String>> {
    let explicit = std::env::var_os("SAFE_GIT_SSH_CONFIG");
    let path = explicit
        .clone()
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|p| p.join(".ssh/config")));
    let Some(path) = path else {
        return Ok(HashMap::new());
    };
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if explicit.is_none() && e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HashMap::new());
        }
        Err(e) => return Err(e).context("read host SSH configuration"),
    };
    let mut text = String::new();
    file.take(1024 * 1024 + 1).read_to_string(&mut text)?;
    ensure!(text.len() <= 1024 * 1024, "SSH configuration exceeds limit");
    parse(&text, host)
}
fn path(value: &str, remote: &Remote) -> Result<PathBuf> {
    let home = std::env::home_dir().context("missing host home")?;
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('d') => out.push_str(home.to_str().context("non-UTF-8 host home")?),
            Some('h') => out.push_str(&remote.host),
            Some('r') => out.push_str(&remote.user),
            Some('p') => out.push_str(&remote.port.to_string()),
            _ => bail!("unsupported SSH path substitution"),
        }
    }
    let path = if let Some(rest) = out.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(out)
    };
    ensure!(
        path.is_absolute(),
        "SSH key and known-hosts paths must be absolute or home-relative"
    );
    Ok(path)
}
pub fn resolve(remote: &Remote, auth: &Auth) -> Result<Settings> {
    let values = config(&remote.host)?;
    let mut resolved = remote.clone();
    if let Some(host) = values.get("hostname") {
        resolved.host = host.replace("%h", &remote.host);
    }
    if resolved.user.is_empty() {
        resolved.user = values
            .get("user")
            .cloned()
            .or_else(|| std::env::var("USER").ok())
            .context("SSH username is required")?;
    }
    if !remote.explicit_port
        && let Some(port) = values.get("port")
    {
        resolved.port = port.parse().context("invalid SSH config port")?;
    }
    ensure!(
        !resolved.user.is_empty()
            && resolved
                .user
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "invalid SSH username"
    );
    ensure!(
        !resolved.host.is_empty()
            && resolved
                .host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-:".contains(&b))
            && resolved.port != 0,
        "invalid SSH destination"
    );
    let identity = if auth.identity.is_some() || auth.agent {
        auth.identity.clone()
    } else {
        values
            .get("identityfile")
            .map(|v| path(v, &resolved))
            .transpose()?
    };
    if let Some(value) = values.get("identitiesonly") {
        ensure!(
            matches!(value.as_str(), "yes" | "no"),
            "invalid IdentitiesOnly value"
        );
        ensure!(
            value != "yes" || identity.is_some() || auth.agent,
            "IdentitiesOnly requires an explicit key or agent selection"
        );
    }
    let known_hosts = if !auth.known_hosts_explicit {
        values
            .get("userknownhostsfile")
            .map(|v| path(v, &resolved))
            .transpose()?
            .unwrap_or_else(|| auth.known_hosts.clone())
    } else {
        auth.known_hosts.clone()
    };
    let jump = values
        .get("proxyjump")
        .filter(|v| v.as_str() != "none")
        .cloned();
    Ok(Settings {
        remote: resolved,
        identity,
        agent: auth.agent,
        known_hosts,
        jump,
    })
}
pub fn route(remote: &Remote, auth: &Auth) -> Result<Vec<Settings>> {
    fn add(
        remote: &Remote,
        auth: &Auth,
        visiting: &mut HashSet<String>,
        out: &mut Vec<Settings>,
    ) -> Result<()> {
        ensure!(
            visiting.len() < 8 && out.len() < 8 && visiting.insert(remote.host.clone()),
            "SSH jump chain is cyclic or too long"
        );
        let settings = resolve(remote, auth)?;
        if let Some(jumps) = &settings.jump {
            for jump in jumps.split(',') {
                ensure!(
                    !jump.is_empty()
                        && !jump.contains('/')
                        && !jump.chars().any(char::is_whitespace),
                    "invalid ProxyJump"
                );
                let remote = Remote::parse(&format!("ssh://{jump}/"))?;
                add(&remote, auth, visiting, out)?;
            }
        }
        visiting.remove(&remote.host);
        out.push(settings);
        Ok(())
    }
    let mut out = Vec::new();
    add(remote, auth, &mut HashSet::new(), &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn host_configuration_is_inert_and_first_value_wins() {
        let text = "Host target\n HostName localhost\n User named\n Port 2222\nHost *\n User other\nMatch exec touch-marker\n HostName bad\nHost !target *\n Port 9999\n";
        let got = parse(text, "target").unwrap();
        assert_eq!(got["hostname"], "localhost");
        assert_eq!(got["user"], "named");
        assert_eq!(got["port"], "2222");
        assert!(parse("Host target\nProxyCommand touch marker", "target").is_err());
        assert!(
            parse("Include /repository/config", "target")
                .unwrap()
                .is_empty()
        );
    }
}
