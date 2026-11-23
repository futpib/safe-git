//! File operations use the same validated snapshots and recovery journal as checkout.
use crate::{
    Cli,
    checkout::{self, Edit, Work},
    index::{self, Entries},
    porcelain,
    repo::{Pending, Repo, Store},
    workflow,
};
use anyhow::{Context, Result, bail, ensure};
use std::{collections::BTreeSet, io::Write};

pub fn run(
    cli: &Cli,
    repo: &Repo,
    store: &mut Store<'_>,
    command: &str,
    args: &[String],
) -> Result<()> {
    ensure!(
        !repo.exists("safe-git-sequence")?,
        "finish the current operation first"
    );
    let (mut force, mut dry, mut cached, mut recursive, mut ignored, mut only_ignored) =
        (false, false, false, false, false, false);
    let mut names = Vec::new();
    let mut end = false;
    let mut quiet = cli.quiet;
    let mut expanded = Vec::new();
    let mut literal_args = false;
    for arg in args {
        if arg == "--" {
            literal_args = true;
        }
        if !literal_args
            && arg.starts_with('-')
            && !arg.starts_with("--")
            && arg.len() > 2
            && arg[1..].bytes().all(|b| b"fnqrdxX".contains(&b))
        {
            expanded.extend(arg[1..].chars().map(|c| format!("-{c}")));
        } else {
            expanded.push(arg.clone());
        }
    }
    for arg in &expanded {
        if end {
            names.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => end = true,
            "-f" | "--force" => force = true,
            "-n" | "--dry-run" => dry = true,
            "-q" | "--quiet" => quiet = true,
            "--cached" if command == "rm" => cached = true,
            "-r" if command == "rm" => recursive = true,
            "-d" if command == "clean" => recursive = true,
            "-x" if command == "clean" => ignored = true,
            "-X" if command == "clean" => {
                ignored = true;
                only_ignored = true;
            }
            a if a.starts_with('-') => bail!("unsupported {command} option {a}"),
            _ => names.push(arg.clone()),
        }
    }
    let staged = porcelain::index(repo)?;
    let mut next = staged.clone();
    let work = Work::new(repo)?;
    let mut actual = checkout::snapshot(repo, &staged)?;
    let mut after = actual.clone();
    let mut report = Vec::new();
    let mut directories = BTreeSet::new();
    if command == "mv" {
        ensure!(names.len() >= 2, "mv requires sources and a destination");
        let destination = names.pop().unwrap();
        let dest = literal(cli, &destination)?;
        let directory = work.state(&dest)?.is_some_and(|e| e.mode == 0o40000);
        ensure!(
            names.len() == 1 || directory,
            "multiple sources require a directory"
        );
        let mut moves = Vec::new();
        for source in names {
            let source = literal(cli, &source)?;
            let target = if directory {
                [
                    dest.as_slice(),
                    b"/",
                    source.rsplit(|b| *b == b'/').next().unwrap(),
                ]
                .concat()
            } else {
                dest.clone()
            };
            index::path(&target)?;
            ensure!(
                source != target && !target.starts_with(&[source.as_slice(), b"/"].concat()),
                "invalid move destination"
            );
            let mut files: Vec<_> = staged
                .keys()
                .filter(|p| workflow::selected(p, std::slice::from_ref(&source)))
                .cloned()
                .collect();
            ensure!(!files.is_empty(), "source is not tracked");
            if work.state(&source)?.is_some_and(|e| e.mode == 0o40000) {
                let (extras, dirs) =
                    workflow::scan_untracked(repo, &staged, std::slice::from_ref(&source), true)?;
                let mut snapshot = Entries::new();
                for path in extras {
                    let e = work.state(&path)?.context("source disappeared")?;
                    ensure!(
                        e.mode != 0o40000,
                        "nested repositories must be moved explicitly"
                    );
                    snapshot.insert(path, e);
                }
                actual.extend(checkout::snapshot(repo, &snapshot)?);
                after.extend(snapshot.iter().map(|(p, e)| (p.clone(), *e)));
                files.extend(snapshot.into_keys());
                directories.extend(dirs);
                directories.insert(source.clone());
            }
            for path in files {
                ensure!(
                    staged.get(&path).is_none_or(|e| e.mode != 0o160000),
                    "move submodules through explicit submodule operations"
                );
                let to = [target.as_slice(), &path[source.len()..]].concat();
                index::path(&to)?;
                let entry = actual.get(&path).copied().context("source is missing")?;
                let existing = work.state(&to)?;
                ensure!(
                    existing.is_none() || (force && staged.contains_key(&to)),
                    "destination exists; tracked destinations require -f"
                );
                ensure!(
                    !moves.iter().any(|(_, d, _)| *d == to),
                    "duplicate move destination"
                );
                moves.push((path, to, entry));
            }
        }
        let sources: BTreeSet<_> = moves.iter().map(|(p, _, _)| p.clone()).collect();
        ensure!(
            !moves.iter().any(|(_, to, _)| sources.contains(to)),
            "overlapping moves are unsupported"
        );
        for (from, to, entry) in moves {
            next.remove(&from);
            if let Some(entry) = staged.get(&from) {
                next.insert(to.clone(), *entry);
            }
            after.remove(&from);
            after.insert(to.clone(), entry);
            report.push(format!(
                "{} -> {}",
                workflow::quote_path(&from),
                workflow::quote_path(&to)
            ));
        }
    } else if command == "rm" {
        ensure!(!names.is_empty(), "rm requires paths");
        let specs = workflow::paths(&names, &cli.prefix)?;
        let base = index::tree(store, repo.read_ref("HEAD")?)?;
        for spec in &specs {
            ensure!(
                staged
                    .keys()
                    .any(|p| workflow::selected(p, std::slice::from_ref(spec))),
                "pathspec did not match tracked files"
            );
            ensure!(
                recursive || staged.contains_key(spec) || spec.iter().any(|b| b"*?[".contains(b)),
                "directory removal requires -r"
            );
        }
        for (path, entry) in &staged {
            if !workflow::selected(path, &specs) {
                continue;
            }
            ensure!(
                cached || entry.mode != 0o160000,
                "use rm --cached to unstage a submodule without deleting its contents"
            );
            let unchanged = actual.get(path) == Some(entry);
            let committed = base.get(path) == Some(entry);
            ensure!(
                force
                    || (if cached {
                        unchanged || committed
                    } else {
                        (unchanged || !actual.contains_key(path)) && committed
                    }),
                "local or staged changes would be lost; use -f explicitly"
            );
            next.remove(path);
            if !cached {
                after.remove(path);
            }
            report.push(workflow::quote_path(path));
        }
    } else {
        ensure!(force || dry, "clean requires -n or an explicit -f");
        if names.is_empty() {
            names.push(".".into());
        }
        let specs = workflow::paths(&names, &cli.prefix)?;
        let visible = workflow::untracked(repo, &staged, &specs, false)?;
        let (candidates, dirs) = workflow::scan_untracked(repo, &staged, &specs, ignored)?;
        if recursive || names != ["."] {
            directories = dirs;
            if only_ignored {
                let (_, visible_dirs) = workflow::scan_untracked(repo, &staged, &specs, false)?;
                directories.retain(|d| !visible_dirs.contains(d));
            }
        }
        let mut targets = Entries::new();
        for path in candidates {
            if only_ignored && visible.contains(&path) {
                continue;
            }
            if !recursive
                && path.iter().enumerate().any(|(i, b)| {
                    *b == b'/'
                        && !staged.keys().any(|p| p.starts_with(&path[..=i]))
                        && !specs
                            .iter()
                            .any(|s| s == &path || s.starts_with(&path[..=i]))
                })
            {
                continue;
            }
            let Some(entry) = work.state(&path)? else {
                continue;
            };
            // The scanner reports nested repositories as directories. Never remove them.
            if entry.mode == 0o40000 {
                continue;
            }
            targets.insert(path.clone(), entry);
            report.push(workflow::quote_path(&path));
        }
        actual.extend(checkout::snapshot(repo, &targets)?);
    }
    index::no_prefix_conflicts(&next)?;
    if !dry && !report.is_empty() {
        if cached {
            let mut lock = Pending::new(repo.root.try_clone()?, "index.lock".into())?;
            ensure!(
                porcelain::index(repo)? == staged,
                "index changed concurrently"
            );
            lock.file
                .write_all(&index::encode_for(&next, repo.format)?)?;
            lock.commit("index")?;
        } else {
            checkout::edit(
                repo,
                store,
                Edit {
                    head: repo.head()?,
                    tip: repo.read_ref("HEAD")?.unwrap_or(repo.format.zero()),
                    before: actual,
                    after,
                    index_before: staged,
                    index_after: next,
                    conflicts_before: Default::default(),
                    conflicts_after: Default::default(),
                },
            )?;
        }
    }
    for path in directories.iter().rev() {
        let (dir, name) = work.parent(path)?;
        if !dry {
            match rustix::fs::unlinkat(&dir, name.as_slice(), rustix::fs::AtFlags::REMOVEDIR) {
                Ok(()) => {}
                Err(rustix::io::Errno::NOTEMPTY)
                | Err(rustix::io::Errno::EXIST)
                | Err(rustix::io::Errno::NOENT) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        report.push(format!("{}/", workflow::quote_path(path)));
    }
    if !quiet {
        for line in report {
            println!("{}{} {line}", if dry { "Would " } else { "" }, command);
        }
    }
    Ok(())
}
fn literal(cli: &Cli, name: &str) -> Result<Vec<u8>> {
    let value = workflow::paths(&[format!(":(literal){name}")], &cli.prefix)?.remove(0);
    let value = value.strip_prefix(b":(literal)").unwrap().to_vec();
    index::path(&value)?;
    Ok(value)
}
