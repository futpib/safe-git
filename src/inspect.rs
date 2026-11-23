//! Bounded history inspection. Repository-controlled pagers, attributes and diffs are inert.
use crate::{
    Cli, compat, index,
    object::{Kind, MAX_OBJECTS, Oid, ReadObjects},
    porcelain,
    repo::{Repo, Store},
    revision, workflow,
};
use anyhow::{Context, Result, bail, ensure};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

pub fn run(
    cli: &Cli,
    repo: &Repo,
    store: &mut Store<'_>,
    command: &str,
    args: &[String],
) -> Result<()> {
    if command == "blame" {
        return blame(cli, repo, store, args);
    }
    let mut values = Vec::new();
    let mut options = Vec::new();
    let mut paths = Vec::new();
    let mut end = false;
    let (mut patch, mut stat) = (true, false);
    for arg in args {
        if end {
            paths.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => end = true,
            "--no-patch" | "-s" => patch = false,
            "--patch" | "-p" => patch = true,
            "--stat" => {
                stat = true;
                patch = false;
            }
            a if a.starts_with('-') => options.push(arg.clone()),
            _ => values.push(arg.clone()),
        }
    }
    if values.is_empty() {
        values.push("HEAD".into());
    }
    for value in values {
        let id = resolve_file(cli, repo, store, &value)?;
        let obj = store.get(id)?;
        match obj.kind {
            Kind::Blob => print!("{}", compat::text(&String::from_utf8_lossy(&obj.data))),
            Kind::Tree => {
                let entries = index::tree(store, Some(id))?;
                for (path, entry) in entries {
                    println!(
                        "{:06o} {} {}\t{}",
                        entry.mode,
                        if entry.mode == 0o160000 {
                            "commit"
                        } else {
                            "blob"
                        },
                        entry.oid,
                        workflow::quote_path(&path)
                    );
                }
            }
            Kind::Tag => {
                println!("{}", compat::text(&String::from_utf8_lossy(&obj.data)));
                let target = revision::peel(store, id)?;
                if store.get(target)?.kind == Kind::Commit {
                    show_commit(cli, repo, store, target, &options, &paths, (patch, stat))?;
                }
            }
            Kind::Commit => show_commit(cli, repo, store, id, &options, &paths, (patch, stat))?,
        }
    }
    Ok(())
}
fn show_commit(
    cli: &Cli,
    repo: &Repo,
    store: &mut Store<'_>,
    id: Oid,
    options: &[String],
    paths: &[String],
    display: (bool, bool),
) -> Result<()> {
    let (patch, stat) = display;
    let mut log = options.to_vec();
    log.extend(["-1".into(), id.to_string()]);
    compat::history(repo, store, &log, false, &cli.prefix)?;
    if patch || stat {
        let parents = store.parents(id)?;
        let mut diff = vec![
            parents
                .first()
                .map(ToString::to_string)
                .unwrap_or(repo.format.empty_tree()?.to_string()),
            id.to_string(),
        ];
        if stat {
            diff.push("--stat".into());
        }
        if !paths.is_empty() {
            diff.push("--".into());
            diff.extend(paths.iter().cloned());
        }
        compat::diff(repo, store, &diff)?;
    }
    Ok(())
}
pub fn resolve_file(cli: &Cli, repo: &Repo, store: &mut Store<'_>, value: &str) -> Result<Oid> {
    if let Some((revision, path)) = value.split_once(':') {
        let prefix = if path.starts_with("./") || path.starts_with("../") {
            cli.prefix.as_path()
        } else {
            std::path::Path::new("")
        };
        let path = workflow::paths(&[format!(":(literal){path}")], prefix)?.remove(0);
        let path = path.strip_prefix(b":(literal)").unwrap();
        let entries = if revision.is_empty() {
            porcelain::index(repo)?
        } else {
            let id = revision::resolve(repo, store, revision)?;
            porcelain::tree(store, id)?
        };
        return Ok(entries
            .get(path)
            .context("path does not exist at that revision")?
            .oid);
    }
    revision::resolve(repo, store, value)
}
pub fn history_ids(
    repo: &Repo,
    store: &mut Store<'_>,
    values: &[&str],
    all: bool,
    first: bool,
) -> Result<Vec<Oid>> {
    let mut tips = Vec::new();
    let mut excluded = HashSet::new();
    if all {
        for id in repo.refs()?.values() {
            let id = revision::peel(store, *id)?;
            if store.get(id)?.kind == Kind::Commit {
                tips.push(id);
            }
        }
    }
    for value in values {
        if let Some(value) = value.strip_prefix('^') {
            let id = revision::commit(repo, store, value)?;
            excluded.extend(crate::object::commits(store, &[id])?);
        } else if let Some((a, b)) = value.split_once("...") {
            let a = revision::commit(repo, store, if a.is_empty() { "HEAD" } else { a })?;
            let b = revision::commit(repo, store, if b.is_empty() { "HEAD" } else { b })?;
            let left = crate::object::commits(store, &[a])?;
            let right = crate::object::commits(store, &[b])?;
            excluded.extend(left.intersection(&right).copied());
            tips.extend([a, b]);
        } else if let Some((a, b)) = value.split_once("..") {
            let a = revision::commit(repo, store, if a.is_empty() { "HEAD" } else { a })?;
            excluded.extend(crate::object::commits(store, &[a])?);
            tips.push(revision::commit(
                repo,
                store,
                if b.is_empty() { "HEAD" } else { b },
            )?);
        } else {
            tips.push(revision::commit(repo, store, value)?);
        }
    }
    if tips.is_empty() && !all {
        tips.push(revision::commit(repo, store, "HEAD")?);
    }
    let mut graph = BTreeMap::new();
    let mut pending = tips;
    while let Some(id) = pending.pop() {
        if excluded.contains(&id) || graph.contains_key(&id) {
            continue;
        }
        ensure!(
            graph.len() < MAX_OBJECTS && pending.len() < MAX_OBJECTS * 4,
            "history exceeds limit"
        );
        let mut parents = store.parents(id)?;
        if first {
            parents.truncate(1);
        }
        pending.extend(&parents);
        graph.insert(id, parents);
    }
    let mut children: HashMap<Oid, usize> = graph.keys().map(|id| (*id, 0)).collect();
    for parents in graph.values() {
        for id in parents {
            if let Some(n) = children.get_mut(id) {
                *n += 1;
            }
        }
    }
    let mut ready = BTreeSet::new();
    for (id, n) in &children {
        if *n == 0 {
            ready.insert((timestamp(&store.get(*id)?.data), *id));
        }
    }
    let mut out = Vec::new();
    while let Some((_, id)) = ready.pop_last() {
        out.push(id);
        for p in &graph[&id] {
            if let Some(n) = children.get_mut(p) {
                *n -= 1;
                if *n == 0 {
                    ready.insert((timestamp(&store.get(*p)?.data), *p));
                }
            }
        }
    }
    ensure!(out.len() == graph.len(), "cyclic commit graph");
    Ok(out)
}
fn timestamp(data: &[u8]) -> i64 {
    data.split(|b| *b == b'\n')
        .find_map(|l| l.strip_prefix(b"committer "))
        .and_then(|l| std::str::from_utf8(l).ok())
        .and_then(|l| l.rsplit(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}
pub fn changed(store: &mut Store<'_>, id: Oid, specs: &[Vec<u8>]) -> Result<bool> {
    let current = porcelain::tree(store, id)?;
    let parents = store.parents(id)?;
    if parents.is_empty() {
        return Ok(current.keys().any(|p| workflow::selected(p, specs)));
    }
    for p in parents {
        let before = porcelain::tree(store, p)?;
        if before
            .keys()
            .chain(current.keys())
            .filter(|p| workflow::selected(p, specs))
            .all(|p| before.get(p) == current.get(p))
        {
            return Ok(false);
        }
    }
    Ok(true)
}
pub fn decorations(repo: &Repo) -> Result<HashMap<Oid, Vec<String>>> {
    let mut out: HashMap<Oid, Vec<String>> = HashMap::new();
    let head = repo.head()?;
    for (name, id) in repo.refs()? {
        let short = name
            .strip_prefix("refs/heads/")
            .or_else(|| name.strip_prefix("refs/remotes/"))
            .unwrap_or(&name);
        let label = if name == head {
            format!("HEAD -> {short}")
        } else if let Some(tag) = name.strip_prefix("refs/tags/") {
            format!("tag: {tag}")
        } else {
            short.into()
        };
        out.entry(id).or_default().push(label);
    }
    if head == "HEAD"
        && let Some(id) = repo.read_ref("HEAD")?
    {
        out.entry(id).or_default().push("HEAD".into());
    }
    Ok(out)
}
#[derive(Default)]
pub struct Graph {
    lanes: Vec<Oid>,
}
impl Graph {
    pub fn continuation(&self) -> String {
        "| ".repeat(self.lanes.len())
    }
    pub fn prefix(&mut self, id: Oid, parents: &[Oid]) -> Result<(String, Option<String>)> {
        ensure!(
            self.lanes.len() + parents.len() <= 256,
            "graph display width exceeds limit"
        );
        let at = self.lanes.iter().position(|p| *p == id).unwrap_or_else(|| {
            self.lanes.push(id);
            self.lanes.len() - 1
        });
        let prefix = (0..self.lanes.len())
            .map(|i| if i == at { "* " } else { "| " })
            .collect();
        self.lanes.remove(at);
        let mut inserted = 0;
        let mut joined = false;
        for p in parents {
            if self.lanes.contains(p) {
                joined = true;
            } else {
                self.lanes.insert(at + inserted, *p);
                inserted += 1;
            }
        }
        let transition = if inserted > 1 {
            Some(format!(
                "{}|{}",
                "| ".repeat(at),
                "\\ ".repeat(inserted - 1)
            ))
        } else if joined {
            Some(format!("{}|/", "| ".repeat(at.saturating_sub(1))))
        } else {
            None
        };
        Ok((prefix, transition))
    }
}
fn blame(cli: &Cli, repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    let mut values = Vec::new();
    let mut porcelain_output = false;
    let mut range = None;
    let mut end = false;
    let mut items = args.iter();
    while let Some(arg) = items.next() {
        if end {
            values.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => end = true,
            "--porcelain" | "--line-porcelain" => porcelain_output = true,
            "-L" => range = Some(items.next().context("missing line range")?.as_str()),
            "-l" | "--long" => {}
            a if a.starts_with('-') => bail!("unsupported blame option {a}"),
            _ => values.push(arg),
        }
    }
    ensure!(
        !values.is_empty() && values.len() <= 2,
        "blame requires [REV] -- PATH"
    );
    let name = values.pop().unwrap();
    let id = revision::commit(repo, store, values.first().copied().unwrap_or("HEAD"))?;
    let paths = workflow::paths(&[format!(":(literal){name}")], &cli.prefix)?;
    let path = paths[0].strip_prefix(b":(literal)").unwrap();
    let entry = porcelain::tree(store, id)?
        .get(path)
        .copied()
        .context("path does not exist")?;
    ensure!(entry.mode != 0o160000, "cannot blame a submodule");
    let actual = if values.is_empty() && repo.worktree.is_some() {
        crate::checkout::snapshot(repo, &[(path.to_vec(), entry)].into())?
            .get(path)
            .copied()
            .context("working file is missing")?
    } else {
        entry
    };
    let initial = if actual != entry { Oid::ZERO } else { id };
    let data = store.get(actual.oid)?.data.clone();
    ensure!(!data.contains(&0), "cannot blame binary data");
    let lines: Vec<_> = data.split_inclusive(|b| *b == b'\n').collect();
    ensure!(lines.len() <= 200_000, "blame line limit exceeded");
    if lines.is_empty() {
        return Ok(());
    }
    let mut owners = vec![(initial, 0usize); lines.len()];
    let mut pending = vec![(
        initial,
        (0..lines.len()).map(|n| (n, n)).collect::<Vec<_>>(),
    )];
    let mut steps = 0;
    let mut line_budget = 64_000_000usize;
    while let Some((commit, mut unresolved)) = pending.pop() {
        steps += 1;
        line_budget = line_budget
            .checked_sub(unresolved.len())
            .context("blame line traversal exceeds limit")?;
        ensure!(steps <= MAX_OBJECTS, "blame traversal limit exceeded");
        let (current, parents) = if commit.is_zero() {
            (store.get(actual.oid)?, vec![id])
        } else {
            let tree = porcelain::tree(store, commit)?;
            (
                store.get(tree.get(path).context("missing blame path")?.oid)?,
                store.parents(commit)?,
            )
        };
        for parent in parents {
            if unresolved.is_empty() {
                break;
            }
            let Some(prior) = porcelain::tree(store, parent)?.get(path).copied() else {
                continue;
            };
            if prior.mode == 0o160000 {
                continue;
            }
            let prior = store.get(prior.oid)?;
            compat::diff_budget(&prior.data, &current.data)?;
            let patch = diffy::create_patch_bytes(&prior.data, &current.data);
            let old_lines = prior.data.split_inclusive(|b| *b == b'\n').count();
            let new_lines = current.data.split_inclusive(|b| *b == b'\n').count();
            let mut mapping = vec![None; new_lines];
            let (mut old, mut new) = (0usize, 0usize);
            for h in patch.hunks() {
                let old_start = h
                    .old_range()
                    .start()
                    .saturating_sub(usize::from(!h.old_range().is_empty()));
                let new_start = h
                    .new_range()
                    .start()
                    .saturating_sub(usize::from(!h.new_range().is_empty()));
                while old < old_start && new < new_start {
                    mapping[new] = Some(old);
                    old += 1;
                    new += 1;
                }
                for line in h.lines() {
                    match line {
                        diffy::Line::Context(_) => {
                            mapping[new] = Some(old);
                            old += 1;
                            new += 1;
                        }
                        diffy::Line::Delete(_) => old += 1,
                        diffy::Line::Insert(_) => new += 1,
                    }
                }
            }
            while old < old_lines && new < new_lines {
                mapping[new] = Some(old);
                old += 1;
                new += 1;
            }
            let mut carried = Vec::new();
            unresolved.retain(|(final_line, current_line)| {
                if let Some(old) = mapping[*current_line] {
                    carried.push((*final_line, old));
                    false
                } else {
                    true
                }
            });
            if !carried.is_empty() {
                pending.push((parent, carried));
            }
        }
        for (line, origin) in unresolved {
            owners[line] = (commit, origin + 1);
        }
    }
    let (start, end) = if let Some(range) = range {
        let (a, b) = range.split_once(',').context("use -L START,END")?;
        (a.parse::<usize>()?, b.parse::<usize>()?)
    } else {
        (1, lines.len())
    };
    ensure!(
        start > 0 && end <= lines.len() && start <= end,
        "invalid blame range"
    );
    for n in start - 1..end {
        let (owner, line) = owners[n];
        let (author, subject) = if owner.is_zero() {
            (
                "Not Committed Yet <not.committed.yet> 0 +0000".into(),
                "Version of file from working tree".into(),
            )
        } else {
            let obj = store.get(owner)?;
            (porcelain::author(&obj.data)?, porcelain::subject(&obj.data))
        };
        if porcelain_output {
            println!(
                "{owner} {line} {} 1\nauthor {}\nsummary {}\nfilename {}",
                n + 1,
                compat::text(author.split(" <").next().unwrap_or(&author)),
                subject,
                workflow::quote_path(path)
            );
            print!("\t{}", compat::text(&String::from_utf8_lossy(lines[n])));
        } else {
            print!(
                "{} ({}, {}) {}",
                crate::output::short(owner),
                compat::text(&author),
                n + 1,
                compat::text(&String::from_utf8_lossy(lines[n]))
            );
        }
        if !lines[n].ends_with(b"\n") {
            println!();
        }
    }
    Ok(())
}
