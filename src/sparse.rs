//! Sparse patterns are bounded data. File changes use the normal recovery journal.
use crate::{
    Cli,
    checkout::{self, Edit, Work},
    config, ignore,
    index::{self, Entries},
    object::Oid,
    porcelain,
    repo::{Pending, Repo, Store},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::BTreeSet,
    io::{Read, Write},
};
const PATTERNS: &str = "info/sparse-checkout";
pub struct Selection {
    rules: Vec<ignore::Rule>,
    budget: usize,
}
impl Selection {
    pub fn includes(&mut self, path: &[u8]) -> Result<bool> {
        ignore::included(&self.rules, path, &mut self.budget)
    }
}
fn selection(bytes: &[u8]) -> Result<Selection> {
    let mut rules = Vec::new();
    ignore::extend(&mut rules, b"", bytes)?;
    Ok(Selection {
        rules,
        budget: 64 * 1024 * 1024,
    })
}
pub fn read(repo: &Repo) -> Result<Option<Selection>> {
    if config::get(repo, &[], "core.sparsecheckout")?.as_deref() != Some("true") {
        return Ok(None);
    }
    Ok(Some(selection(
        &repo
            .read(PATTERNS, 1024 * 1024)?
            .context("missing sparse patterns")?,
    )?))
}
pub fn mark(repo: &Repo, entries: &mut Entries) -> Result<()> {
    if let Some(mut selection) = read(repo)? {
        let work = Work::new(repo)?;
        for (path, entry) in entries {
            if entry.flags & index::INTENT == 0
                && !selection.includes(path)?
                && work.state(path)?.is_none()
            {
                entry.flags |= index::SKIP;
            }
        }
    }
    Ok(())
}
fn cone_paths(bytes: &[u8]) -> Result<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    let lines: BTreeSet<_> = std::str::from_utf8(bytes)?.lines().collect();
    for line in &lines {
        if line.starts_with('/') && line.ends_with('/') && !line.contains('*') {
            let path = line.trim_matches('/');
            index::path(path.as_bytes())?;
            if !lines.contains(format!("!/{path}/*/").as_str()) {
                paths.insert(path.to_owned());
            }
        }
    }
    Ok(paths)
}
fn cone_pattern(paths: &BTreeSet<String>) -> Vec<u8> {
    let mut patterns = BTreeSet::new();
    for path in paths {
        if paths
            .iter()
            .any(|parent| path.starts_with(&format!("{parent}/")))
        {
            continue;
        }
        for (i, _) in path.bytes().enumerate().filter(|(_, b)| *b == b'/') {
            let parent = &path[..i];
            patterns.insert(format!("/{parent}/\n!/{parent}/*/\n"));
        }
        patterns.insert(format!("/{path}/\n"));
    }
    format!("/*\n!/*/\n{}", patterns.into_iter().collect::<String>()).into_bytes()
}
pub fn run(cli: &Cli, repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    let command = args.first().map(String::as_str).unwrap_or("list");
    let old_patterns = repo.read(PATTERNS, 1024 * 1024)?;
    let old_config = repo
        .read("config", 8 * 1024 * 1024)?
        .context("missing config")?;
    let mut cone = config::get(repo, &[], "core.sparsecheckoutcone")?.as_deref() != Some("false");
    let mut paths = Vec::new();
    let mut stdin = false;
    let mut end = false;
    for arg in args.get(1..).unwrap_or_default() {
        if end {
            paths.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => end = true,
            "--cone" => cone = true,
            "--no-cone" => cone = false,
            "--no-sparse-index" => {}
            "--stdin" => stdin = true,
            a if a.starts_with('-') => bail!("unsupported sparse-checkout option {a}"),
            _ => paths.push(arg.clone()),
        }
    }
    if stdin {
        ensure!(paths.is_empty(), "--stdin takes no path arguments");
        let mut bytes = String::new();
        std::io::stdin()
            .take(1024 * 1024 + 1)
            .read_to_string(&mut bytes)?;
        ensure!(bytes.len() <= 1024 * 1024, "sparse input exceeds limit");
        paths.extend(bytes.lines().map(str::to_owned));
    }
    if command == "list" {
        ensure!(paths.is_empty(), "list takes no paths");
        let bytes = old_patterns.context("sparse checkout is not enabled")?;
        if cone {
            for path in cone_paths(&bytes)? {
                println!("{}", crate::compat::text(&path));
            }
        } else {
            print!("{}", crate::compat::text(std::str::from_utf8(&bytes)?));
        }
        return Ok(());
    }
    ensure!(
        matches!(command, "init" | "set" | "add" | "reapply" | "disable"),
        "unsupported sparse-checkout command"
    );
    ensure!(
        matches!(command, "set" | "add") || paths.is_empty(),
        "command takes no paths"
    );
    let disable = command == "disable";
    let bytes = if disable {
        Vec::new()
    } else if command == "reapply" {
        old_patterns
            .clone()
            .context("sparse checkout is not enabled")?
    } else if cone {
        let mut selected = if command == "add" {
            cone_paths(old_patterns.as_deref().unwrap_or_default())?
        } else {
            BTreeSet::new()
        };
        for path in paths {
            let path = cli.prefix.join(path);
            let path = path
                .to_str()
                .context("invalid sparse path")?
                .trim_end_matches('/');
            index::path(path.as_bytes())?;
            ensure!(
                !path.contains(['*', '?', '[', ']', '!', '\\']),
                "cone paths must be literal directories"
            );
            selected.insert(path.to_owned());
        }
        cone_pattern(&selected)
    } else {
        let mut bytes = if command == "add" {
            old_patterns.clone().unwrap_or_default()
        } else {
            Vec::new()
        };
        for path in paths {
            ensure!(
                !path.chars().any(char::is_control),
                "invalid sparse pattern"
            );
            bytes.extend_from_slice(path.as_bytes());
            bytes.push(b'\n');
        }
        bytes
    };
    let mut selection = selection(&bytes)?;
    let index_before = porcelain::index(repo)?;
    let work = Work::new(repo)?;
    let mut actual = Entries::new();
    for (path, entry) in &index_before {
        if entry.mode == 0o160000 || work.state(path)?.is_some() {
            actual.insert(path.clone(), *entry);
        }
    }
    let before = checkout::snapshot(repo, &actual)?;
    let mut after = Entries::new();
    let mut index_after = if !repo.exists("index")? {
        index::tree(store, repo.read_ref("HEAD")?)?
    } else {
        index_before.clone()
    };
    for (path, entry) in &mut index_after {
        if disable || selection.includes(path)? || entry.mode == 0o160000 {
            entry.flags &= !index::SKIP;
            after.insert(path.clone(), before.get(path).copied().unwrap_or(*entry));
        } else {
            ensure!(
                entry.flags & index::INTENT == 0 && before.get(path).is_none_or(|e| e == entry),
                "local changes would be removed by sparse checkout"
            );
            entry.flags |= index::SKIP;
        }
    }
    // Lock and compare configuration before editing. In case of interruption the
    // file journal preserves the index; reapply can finish the sparse selection.
    let mut lock = Pending::new(repo.root.try_clone()?, "config.lock".into())?;
    ensure!(
        repo.read("config", 8 * 1024 * 1024)?.as_deref() == Some(old_config.as_slice()),
        "config changed concurrently"
    );
    let (dir, name) = repo.parent(PATTERNS, true)?;
    let mut pattern_lock = Pending::new(dir, format!("{name}.lock"))?;
    ensure!(
        repo.read(PATTERNS, 1024 * 1024)? == old_patterns,
        "patterns changed concurrently"
    );
    checkout::edit(
        repo,
        store,
        Edit {
            head: repo.head()?,
            tip: repo.read_ref("HEAD")?.unwrap_or(Oid::ZERO),
            before,
            after,
            index_before,
            index_after,
            conflicts_before: Default::default(),
            conflicts_after: Default::default(),
        },
    )?;
    pattern_lock.file.write_all(&bytes)?;
    pattern_lock.commit(&name)?;
    let mut config = old_config;
    config.extend_from_slice(
        format!(
            "\n[core]\n\tsparseCheckout = {}\n\tsparseCheckoutCone = {cone}\n",
            !disable
        )
        .as_bytes(),
    );
    lock.file.write_all(&config)?;
    lock.commit("config")
}
