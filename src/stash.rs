//! Git-compatible stash commits and reflogs; all worktree edits use recovery.
use crate::{
    Cli,
    checkout::{self, Edit},
    object::{Oid, ReadObjects},
    porcelain,
    repo::{Pending, Repo, Store},
};
use anyhow::{Context, Result, bail, ensure};
use std::io::Write;

const LOG: &str = "logs/refs/stash";
const LIMIT: usize = 8 * 1024 * 1024;

fn stack(repo: &Repo) -> Result<Vec<Vec<u8>>> {
    let data = repo.read(LOG, LIMIT)?.unwrap_or_default();
    let lines: Vec<_> = data
        .split_inclusive(|b| *b == b'\n')
        .map(Vec::from)
        .collect();
    ensure!(lines.len() <= 10000, "stash entry limit exceeded");
    for line in &lines {
        ensure!(
            line.len() >= 84
                && line[40] == b' '
                && line[81] == b' '
                && line.ends_with(b"\n")
                && line.contains(&b'\t'),
            "invalid stash reflog"
        );
        Oid::parse(&line[..40])?;
        Oid::parse(&line[41..81])?;
    }
    if let Some(last) = lines.last() {
        ensure!(
            repo.read_ref("refs/stash")? == Some(Oid::parse(&last[41..81])?),
            "stash ref and reflog disagree"
        );
    } else {
        ensure!(
            repo.read_ref("refs/stash")?.is_none(),
            "stash ref has no reflog; refusing to overwrite it"
        );
    }
    Ok(lines)
}

fn record(
    repo: &Repo,
    previous: Option<Oid>,
    new: Oid,
    identity: &str,
    message: &str,
) -> Result<()> {
    let reference = repo.prepare_ref("refs/stash", previous)?;
    let (dir, name) = repo.parent(LOG, true)?;
    let mut log = Pending::new(dir, format!("{name}.lock"))?;
    let mut bytes = stack(repo)?.concat();
    bytes.extend_from_slice(
        format!(
            "{} {new} {identity}\t{message}\n",
            previous.unwrap_or(Oid::ZERO)
        )
        .as_bytes(),
    );
    ensure!(bytes.len() <= LIMIT, "stash reflog exceeds limit");
    log.file.write_all(&bytes)?;
    // Publish the commit before cleaning anything. A failure retains the data;
    // an interrupted metadata update is detected by stack() on the next call.
    reference.commit(new)?;
    log.commit(&name)
}
fn drop_entry(repo: &Repo, expected: Oid, selection: usize) -> Result<()> {
    let mut lines = stack(repo)?;
    ensure!(selection < lines.len(), "stash entry does not exist");
    let old_top = Oid::parse(&lines.last().unwrap()[41..81])?;
    let top = lines.remove(lines.len() - 1 - selection);
    let mut previous = Oid::ZERO;
    for line in &mut lines {
        line[..40].copy_from_slice(previous.to_string().as_bytes());
        previous = Oid::parse(&line[41..81])?;
    }
    ensure!(
        Oid::parse(&top[41..81])? == expected,
        "stash changed concurrently"
    );
    let (dir, name) = repo.parent(LOG, true)?;
    let mut log = Pending::new(dir, format!("{name}.lock"))?;
    log.file.write_all(&lines.concat())?;
    if let Some(last) = lines.last() {
        repo.update_ref("refs/stash", Some(old_top), Oid::parse(&last[41..81])?)?;
    } else {
        repo.delete_ref("refs/stash", old_top)?;
    }
    log.commit(&name)
}

pub fn run(cli: &Cli, repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    let mut items = args.iter().peekable();
    let command = if items.peek().is_some_and(|a| !a.starts_with('-')) {
        items.next().unwrap().as_str()
    } else {
        "push"
    };
    let mut message = None;
    let mut include_untracked = false;
    let mut include_ignored = false;
    let mut selection = 0usize;
    let mut patch = false;
    let mut restore_index = false;
    let mut quiet = cli.quiet;
    while let Some(arg) = items.next() {
        match arg.as_str() {
            "-m" | "--message" if command == "push" => {
                message = Some(items.next().context("missing stash message")?.as_str())
            }
            "--index" if matches!(command, "pop" | "apply") => restore_index = true,
            "-q" | "--quiet" => quiet = true,
            "-u" | "--include-untracked" if command == "push" => include_untracked = true,
            "-a" | "--all" if command == "push" => {
                include_untracked = true;
                include_ignored = true;
            }
            "-p" | "--patch" if command == "show" => patch = true,
            "--stat" if command == "show" => {}
            a if matches!(command, "pop" | "apply" | "drop" | "show") => {
                selection = a
                    .strip_prefix("stash@{")
                    .and_then(|v| v.strip_suffix('}'))
                    .unwrap_or(a)
                    .parse()
                    .context("invalid stash selector")?;
            }
            _ => bail!("unsupported stash option {arg}"),
        }
    }
    match command {
        "push" => {
            let head_name = repo.head()?;
            let head = repo
                .read_ref(&head_name)?
                .context("cannot stash before the first commit")?;
            let staged = porcelain::index(repo)?;
            let working = checkout::snapshot(repo, &staged)?;
            let mut extra = crate::index::Entries::new();
            if include_untracked {
                for path in
                    crate::workflow::untracked(repo, &staged, &[Vec::new()], include_ignored)?
                {
                    extra.insert(
                        path,
                        crate::index::Entry {
                            mode: 0o100644,
                            oid: Oid::ZERO,
                        },
                    );
                }
                extra = checkout::snapshot(repo, &extra)?;
            }
            let base = porcelain::tree(store, head)?;
            if staged == base && working == base && extra.is_empty() {
                if !quiet {
                    println!("No local changes to save");
                }
                return Ok(());
            }
            // Validate before creating the record; hooks and signing are never consulted.
            let identity = porcelain::identity(repo, &cli.config, "COMMITTER")?;
            let branch = repo.head()?;
            let label = branch.strip_prefix("refs/heads/").unwrap_or("(no branch)");
            let base_subject = format!(
                "{} {}",
                crate::output::short(head),
                porcelain::subject(&store.get(head)?.data)
            );
            let subject = message.unwrap_or(&base_subject);
            let subject = crate::compat::text(subject).replace(['\n', '\r', '\t'], " ");
            let title = format!(
                "{} {label}: {subject}",
                if message.is_some() { "On" } else { "WIP on" }
            );
            let index_tree = porcelain::write_tree(repo, &staged)?;
            let index_commit = porcelain::new_commit(
                repo,
                index_tree,
                &[head],
                &identity,
                &identity,
                format!("index on {label}: {base_subject}").as_bytes(),
            )?;
            let work_tree = porcelain::write_tree(repo, &working)?;
            let mut parents = vec![head, index_commit];
            if include_untracked {
                let tree = porcelain::write_tree(repo, &extra)?;
                parents.push(porcelain::new_commit(
                    repo,
                    tree,
                    &[],
                    &identity,
                    &identity,
                    b"untracked files",
                )?);
            }
            let saved = porcelain::new_commit(
                repo,
                work_tree,
                &parents,
                &identity,
                &identity,
                title.as_bytes(),
            )?;
            let previous = repo.read_ref("refs/stash")?;
            record(repo, previous, saved, &identity, &title)?;
            let mut working = working;
            working.extend(extra);
            checkout::edit(
                repo,
                store,
                Edit {
                    conflicts_before: Default::default(),
                    conflicts_after: Default::default(),
                    head: head_name,
                    tip: head,
                    before: working,
                    after: base.clone(),
                    index_before: staged,
                    index_after: base,
                },
            )?;
            if !quiet {
                println!("Saved working directory and index state {title}");
            }
            Ok(())
        }
        "pop" | "apply" => {
            let head_name = repo.head()?;
            let tip = repo
                .read_ref(&head_name)?
                .context("cannot apply a stash before the first commit")?;
            let lines = stack(repo)?;
            let last = lines
                .iter()
                .rev()
                .nth(selection)
                .context("stash entry does not exist")?;
            let saved = Oid::parse(&last[41..81])?;
            let parents = store.get(saved)?.commit()?.1;
            ensure!((2..=3).contains(&parents.len()), "invalid stash parents");
            let base = porcelain::tree(store, parents[0])?;
            let saved_index = porcelain::tree(store, parents[1])?;
            ensure!(
                store.get(parents[1])?.commit()?.1 == [parents[0]],
                "invalid stash index parent"
            );
            let saved_work = porcelain::tree(store, saved)?;
            let staged = porcelain::index(repo)?;
            let working = checkout::snapshot(repo, &staged)?;
            let mut after = porcelain::three_way(repo, store, &base, &working, &saved_work)?;
            let mut index_after = if restore_index {
                porcelain::three_way(repo, store, &base, &staged, &saved_index)?
            } else {
                staged.clone()
            };
            if !restore_index {
                // Like Git, added files remain staged; changes to existing files
                // return as worktree edits unless --index was requested.
                for (path, entry) in &after {
                    if !base.contains_key(path) && !staged.contains_key(path) {
                        index_after.insert(path.clone(), *entry);
                    }
                }
            }
            if parents.len() == 3 {
                ensure!(
                    store.get(parents[2])?.commit()?.1.is_empty(),
                    "invalid untracked stash parent"
                );
                let extra = porcelain::tree(store, parents[2])?;
                let work = checkout::Work::new(repo)?;
                for path in extra.keys() {
                    ensure!(
                        !after.contains_key(path) && work.state(path)?.is_none(),
                        "untracked stash path already exists"
                    );
                }
                after.extend(extra);
            }
            checkout::edit(
                repo,
                store,
                Edit {
                    conflicts_before: Default::default(),
                    conflicts_after: Default::default(),
                    head: head_name,
                    tip,
                    before: working,
                    after,
                    index_before: staged,
                    index_after,
                },
            )?;
            if command == "pop" {
                drop_entry(repo, saved, selection)?;
            }
            if !quiet {
                println!(
                    "{} stash {saved}",
                    if command == "pop" {
                        "Applied and dropped"
                    } else {
                        "Applied"
                    }
                );
            }
            Ok(())
        }
        "list" => {
            for (i, line) in stack(repo)?.iter().rev().enumerate() {
                let message = line
                    .splitn(2, |b| *b == b'\t')
                    .nth(1)
                    .context("invalid stash message")?;
                println!(
                    "stash@{{{i}}}: {}",
                    crate::compat::text(String::from_utf8_lossy(message).trim_end())
                );
            }
            Ok(())
        }
        "show" => {
            let lines = stack(repo)?;
            let line = lines
                .iter()
                .rev()
                .nth(selection)
                .context("stash entry does not exist")?;
            let saved = Oid::parse(&line[41..81])?;
            let base = *store
                .get(saved)?
                .commit()?
                .1
                .first()
                .context("invalid stash")?;
            crate::compat::diff(
                repo,
                store,
                &[
                    if patch {
                        "--patch".into()
                    } else {
                        "--stat".into()
                    },
                    base.to_string(),
                    saved.to_string(),
                ],
            )
        }
        "clear" => {
            if let Some(old) = repo.read_ref("refs/stash")? {
                stack(repo)?;
                repo.delete_ref("refs/stash", old)?;
            }
            repo.remove(LOG)
        }
        "drop" => {
            let lines = stack(repo)?;
            let last = lines
                .iter()
                .rev()
                .nth(selection)
                .context("stash entry does not exist")?;
            drop_entry(repo, Oid::parse(&last[41..81])?, selection)
        }
        _ => bail!("unsupported stash command {command}"),
    }
}
