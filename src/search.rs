//! Bounded, in-process text searches. Patterns and content never select commands.
use crate::{
    Cli, checkout, compat,
    object::ReadObjects,
    porcelain,
    repo::{Repo, Store},
    revision, workflow,
};
use anyhow::{Context, Result, bail, ensure};
use regex::bytes::{Regex, RegexBuilder};

pub struct Matcher(Vec<Regex>);
impl Matcher {
    pub fn new(
        patterns: &[String],
        fixed: bool,
        extended: bool,
        insensitive: bool,
    ) -> Result<Self> {
        ensure!(patterns.len() <= 128, "too many search patterns");
        let mut result = Vec::new();
        for pattern in patterns {
            ensure!(pattern.len() <= 4096, "search pattern exceeds limit");
            let source = if fixed {
                regex::escape(pattern)
            } else if extended {
                pattern.clone()
            } else {
                basic(pattern)
            };
            result.push(
                RegexBuilder::new(&source)
                    .unicode(false)
                    .multi_line(true)
                    .case_insensitive(insensitive)
                    .size_limit(2 * 1024 * 1024)
                    .dfa_size_limit(2 * 1024 * 1024)
                    .build()?,
            );
        }
        Ok(Self(result))
    }
    pub fn matches(&self, value: &[u8]) -> bool {
        self.0.is_empty() || self.0.iter().any(|r| r.is_match(value))
    }
    pub fn matches_all(&self, value: &[u8]) -> bool {
        self.0.iter().all(|r| r.is_match(value))
    }
}
fn basic(pattern: &str) -> String {
    let mut out = String::new();
    let mut chars = pattern.chars().peekable();
    let mut bracket = false;
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                if !bracket && "()+?|{}".contains(next) {
                    out.push(next);
                } else {
                    out.push('\\');
                    out.push(next);
                }
            } else {
                out.push('\\');
            }
        } else {
            if c == '[' {
                bracket = true;
            }
            if !bracket && "()+?|{}".contains(c) {
                out.push('\\');
            }
            out.push(c);
            if c == ']' {
                bracket = false;
            }
        }
    }
    out
}
pub fn grep(cli: &Cli, repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    let (
        mut cached,
        mut number,
        mut fixed,
        mut extended,
        mut insensitive,
        mut invert,
        mut quiet,
        mut count,
        mut files,
        mut without,
    ) = (
        false, false, false, false, false, false, false, false, false, false,
    );
    let mut patterns = Vec::new();
    let mut names = Vec::new();
    let mut rev = None;
    let mut end = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if end {
            names.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => end = true,
            "--cached" => cached = true,
            "-n" | "--line-number" => number = true,
            "-F" | "--fixed-strings" => fixed = true,
            "-E" | "--extended-regexp" => extended = true,
            "-G" | "--basic-regexp" => extended = false,
            "-i" | "--ignore-case" => insensitive = true,
            "-v" | "--invert-match" => invert = true,
            "-q" | "--quiet" => quiet = true,
            "-c" | "--count" => count = true,
            "-l" | "--files-with-matches" => files = true,
            "-L" | "--files-without-match" => without = true,
            "--no-color" | "--no-ext-grep" | "--full-name" => {}
            "-e" => patterns.push(iter.next().context("missing pattern")?.clone()),
            a if a.starts_with('-') => bail!("unsupported grep option {a}"),
            _ if patterns.is_empty() => patterns.push(arg.clone()),
            _ if rev.is_none()
                && names.is_empty()
                && revision::resolve(repo, store, arg).is_ok() =>
            {
                rev = Some(arg.as_str())
            }
            _ => names.push(arg.clone()),
        }
    }
    ensure!(!patterns.is_empty(), "grep requires a pattern");
    ensure!(
        !cached || rev.is_none(),
        "--cached cannot be combined with a tree"
    );
    let matcher = Matcher::new(&patterns, fixed, extended, insensitive)?;
    let specs = if names.is_empty() {
        vec![Vec::new()]
    } else {
        workflow::paths(&names, &cli.prefix)?
    };
    let mut entries = if let Some(rev) = rev {
        let id = revision::resolve(repo, store, rev)?;
        porcelain::tree(store, id)?
    } else {
        porcelain::index(repo)?
    };
    entries.retain(|p, e| workflow::selected(p, &specs) && matches!(e.mode, 0o100644 | 0o100755));
    if !cached && rev.is_none() {
        entries = checkout::snapshot(repo, &entries)?;
    }
    let mut found = false;
    let mut budget = crate::object::MAX_TOTAL;
    for (path, entry) in entries {
        let object = store.get(entry.oid)?;
        budget = budget
            .checked_sub(object.data.len())
            .context("grep byte budget exceeded")?;
        let label = format!(
            "{}{}",
            rev.map(|r| format!("{}:", compat::text(r)))
                .unwrap_or_default(),
            crate::output::quote(&path)
        );
        let binary = object.data.contains(&0);
        let mut hits = 0;
        for (line, bytes) in object.data.split_inclusive(|b| *b == b'\n').enumerate() {
            let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
            if matcher.matches(bytes) == invert {
                continue;
            }
            hits += 1;
            if !quiet && !count && !files && !without && !binary {
                println!(
                    "{label}:{}{}",
                    if number {
                        format!("{}:", line + 1)
                    } else {
                        String::new()
                    },
                    compat::text(&String::from_utf8_lossy(bytes))
                );
            }
        }
        let matched = if without { hits == 0 } else { hits != 0 };
        found |= matched;
        if quiet && matched {
            return Ok(());
        }
        if !quiet {
            if (files || without) && matched {
                println!("{label}");
            } else if count && hits > 0 {
                println!("{label}:{hits}");
            } else if binary && hits > 0 && !files && !without {
                println!("Binary file {label} matches");
            }
        }
    }
    if found {
        Ok(())
    } else {
        Err(compat::Exit(1).into())
    }
}
