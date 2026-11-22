//! Fixed in-process tree transformations; no attributes, merge drivers, hooks or sequencer scripts.
use crate::{
    checkout::{self, Update},
    config,
    index::{self, Entries, Entry},
    object::{Kind, MAX_OBJECT, MAX_TOTAL, Object, Oid, ReadObjects},
    repo::{Pending, Repo, Store},
    revision,
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
};

pub fn index(repo: &Repo) -> Result<Entries> {
    repo.read("index", checkout::INDEX_LIMIT)?
        .map(|b| index::decode(&b))
        .transpose()
        .map(|v| v.unwrap_or_default())
}
pub fn tree(store: &mut Store<'_>, id: Oid) -> Result<Entries> {
    if id.to_string() == revision::EMPTY_TREE {
        Ok(Entries::new())
    } else {
        let id = revision::peel(store, id)?;
        index::tree(store, Some(id))
    }
}
pub fn write_tree(repo: &Repo, entries: &Entries) -> Result<Oid> {
    index::no_prefix_conflicts(entries)?;
    for name in entries.keys() {
        index::path(name)?;
    }
    fn build(repo: &Repo, entries: Vec<(&[u8], Entry)>, depth: usize) -> Result<Oid> {
        ensure!(depth <= 128, "tree nesting exceeds limit");
        let mut children: BTreeMap<Vec<u8>, Vec<(&[u8], Entry)>> = BTreeMap::new();
        let mut items = Vec::new();
        for (path, entry) in entries {
            if let Some(i) = path.iter().position(|b| *b == b'/') {
                children
                    .entry(path[..i].to_vec())
                    .or_default()
                    .push((&path[i + 1..], entry));
            } else {
                items.push((path.to_vec(), entry));
            }
        }
        for (name, entries) in children {
            items.push((
                name,
                Entry {
                    mode: 0o40000,
                    oid: build(repo, entries, depth + 1)?,
                },
            ));
        }
        items.sort_by_key(|(name, e)| {
            let mut key = name.clone();
            if e.mode == 0o40000 {
                key.push(b'/');
            }
            key
        });
        let mut data = Vec::new();
        for (name, e) in items {
            data.extend_from_slice(format!("{:o} ", e.mode).as_bytes());
            data.extend(name);
            data.push(0);
            data.extend(e.oid.0);
        }
        ensure!(data.len() <= MAX_OBJECT, "tree exceeds limit");
        let obj = Object {
            kind: Kind::Tree,
            data,
        };
        repo.write_object(&obj)?;
        obj.oid()
    }
    build(
        repo,
        entries.iter().map(|(p, e)| (p.as_slice(), *e)).collect(),
        0,
    )
}
pub(crate) fn identity(repo: &Repo, overrides: &[String], role: &str) -> Result<String> {
    let name = std::env::var(format!("GIT_{role}_NAME"))
        .ok()
        .or(config::get(repo, overrides, "user.name")?)
        .context("Author identity unknown; set user.name and user.email")?;
    let email = std::env::var(format!("GIT_{role}_EMAIL"))
        .ok()
        .or(config::get(repo, overrides, "user.email")?)
        .context("Author identity unknown; set user.name and user.email")?;
    ensure!(
        !name.is_empty()
            && !email.is_empty()
            && !name
                .chars()
                .chain(email.chars())
                .any(|c| c.is_control() || c == '<' || c == '>'),
        "invalid author identity"
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let date = std::env::var(format!("GIT_{role}_DATE")).unwrap_or_else(|_| format!("{now} +0000"));
    let date = date.trim_start_matches('@');
    let parts: Vec<_> = date.split_whitespace().collect();
    ensure!(
        parts.len() == 2
            && parts[0].parse::<u64>().is_ok()
            && parts[1].len() == 5
            && matches!(parts[1].as_bytes()[0], b'+' | b'-')
            && parts[1].as_bytes()[1..].iter().all(u8::is_ascii_digit),
        "date must be Unix seconds followed by a numeric timezone"
    );
    Ok(format!("{name} <{email}> {} {}", parts[0], parts[1]))
}
pub(crate) fn new_commit(
    repo: &Repo,
    tree: Oid,
    parents: &[Oid],
    author: &str,
    committer: &str,
    message: &[u8],
) -> Result<Oid> {
    ensure!(
        !author
            .chars()
            .chain(committer.chars())
            .any(char::is_control),
        "invalid commit identity"
    );
    let mut data = format!("tree {tree}\n").into_bytes();
    for id in parents {
        data.extend_from_slice(format!("parent {id}\n").as_bytes());
    }
    data.extend_from_slice(format!("author {author}\ncommitter {committer}\n\n").as_bytes());
    data.extend_from_slice(message);
    if !data.ends_with(b"\n") {
        data.push(b'\n');
    }
    ensure!(data.len() <= MAX_OBJECT, "commit exceeds limit");
    let object = Object {
        kind: Kind::Commit,
        data,
    };
    repo.write_object(&object)?;
    object.oid()
}
pub fn commit(
    repo: &Repo,
    store: &mut Store<'_>,
    overrides: &[String],
    message: &str,
    all: bool,
    quiet: bool,
) -> Result<()> {
    ensure!(!message.trim().is_empty(), "empty commit message");
    for state in [
        "safe-git-checkout",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-apply",
        "rebase-merge",
    ] {
        ensure!(!repo.exists(state)?, "unfinished operation: {state}");
    }
    let head = repo.head()?;
    let old = repo.read_ref(&head)?;
    let ref_lock = repo.prepare_ref(&head, old)?;
    let _head_lock = if head != "HEAD" {
        Some(Pending::new(repo.root.try_clone()?, "HEAD.lock".into())?)
    } else {
        None
    };
    ensure!(repo.head()? == head, "HEAD changed concurrently");
    let head_bytes = repo.read("HEAD", 2048)?.context("missing HEAD")?;
    let mut index_lock = Pending::new(repo.root.try_clone()?, "index.lock".into())?;
    let original = repo.read("index", checkout::INDEX_LIMIT)?;
    let staged = original
        .as_ref()
        .map(|b| index::decode(b))
        .transpose()?
        .unwrap_or_default();
    let entries = if all {
        checkout::snapshot(repo, &staged)?
    } else {
        staged
    };
    let tree = write_tree(repo, &entries)?;
    let before = index::tree(store, old)?;
    if let Some(old) = old {
        ensure!(
            store.get(old)?.commit()?.0 != tree,
            "nothing to commit, working tree clean"
        );
    }
    let id = new_commit(
        repo,
        tree,
        &old.into_iter().collect::<Vec<_>>(),
        &identity(repo, overrides, "AUTHOR")?,
        &identity(repo, overrides, "COMMITTER")?,
        message.as_bytes(),
    )?;
    ensure!(
        repo.read("HEAD", 2048)?.as_deref() == Some(head_bytes.as_slice())
            && repo.read("index", checkout::INDEX_LIMIT)? == original,
        "HEAD or index changed concurrently"
    );
    // Commit records a snapshot. Never check it out over subsequent or unstaged edits.
    index_lock.file.write_all(&index::encode(&entries)?)?;
    index_lock.commit("index")?;
    ref_lock.commit(id)?;
    if !quiet {
        println!(
            "[{} {}] {}",
            head.strip_prefix("refs/heads/").unwrap_or("detached HEAD"),
            crate::output::short(id),
            crate::compat::text(message.lines().next().unwrap_or_default())
        );
    }
    if !quiet {
        match crate::output::diffstat_entries(store, &before, &entries) {
            Ok(stat) => {
                let mut summary = false;
                for line in stat.lines() {
                    if !line.contains('|')
                        && line
                            .split_whitespace()
                            .nth(1)
                            .is_some_and(|v| v == "file" || v == "files")
                    {
                        summary = true;
                    }
                    if summary {
                        println!("{line}");
                    }
                }
            }
            Err(e) => eprintln!(
                "warning: commit summary omitted ({})",
                crate::compat::text(&format!("{e:#}"))
            ),
        }
    }
    Ok(())
}
pub fn reset(repo: &Repo, store: &mut Store<'_>, id: Oid, quiet: bool) -> Result<()> {
    let head = repo.head()?;
    let old = repo.read_ref(&head)?;
    store.get(id)?.commit()?;
    let after = tree(store, id)?;
    checkout::apply(
        repo,
        store,
        Update {
            reference: &head,
            old,
            new: id,
            after,
            force: true,
            head_after: None,
            reset: true,
        },
    )?;
    for path in ["MERGE_HEAD", "MERGE_MSG", "MERGE_MODE"] {
        repo.remove(path)?;
    }
    if !quiet {
        println!(
            "HEAD is now at {} {}",
            crate::output::short(id),
            subject(&store.get(id)?.data)
        );
    }
    Ok(())
}
pub fn checkout(
    repo: &Repo,
    store: &mut Store<'_>,
    value: &str,
    branch: Option<&str>,
    force: bool,
    quiet: bool,
) -> Result<()> {
    let id = revision::commit(repo, store, value)?;
    let branch = if let Some(name) = branch {
        Some(format!("refs/heads/{}", crate::repo::branch(name)?))
    } else {
        let name = format!("refs/heads/{value}");
        if crate::repo::refname(&name).is_ok() && repo.read_ref(&name)?.is_some() {
            Some(name)
        } else {
            None
        }
    };
    let reference = branch.as_deref().unwrap_or("HEAD");
    // A detached checkout must not overwrite the branch referenced by HEAD.
    // prepare_ref("HEAD") replaces HEAD itself with a direct object ID.
    let old = repo.read_ref(reference)?;
    let after = tree(store, id)?;
    checkout::apply(
        repo,
        store,
        Update {
            reference,
            old,
            new: id,
            after,
            force,
            head_after: Some(branch.clone().unwrap_or_else(|| id.to_string())),
            reset: force,
        },
    )?;
    if !quiet {
        if let Some(branch) = branch {
            eprintln!(
                "Switched to branch '{}'",
                branch.trim_start_matches("refs/heads/")
            );
        } else {
            eprintln!(
                "HEAD is now at {} {}",
                crate::output::short(id),
                subject(&store.get(id)?.data)
            );
        }
    }
    Ok(())
}
fn blob(store: &mut Store<'_>, entry: Entry, budget: &mut usize) -> Result<Vec<u8>> {
    let object = store.get(entry.oid)?;
    ensure!(object.kind == Kind::Blob, "expected blob");
    ensure!(object.data.len() <= *budget, "merge byte limit exceeded");
    *budget -= object.data.len();
    Ok(object.data.clone())
}
pub(crate) fn three_way(
    repo: &Repo,
    store: &mut Store<'_>,
    base: &Entries,
    ours: &Entries,
    theirs: &Entries,
) -> Result<Entries> {
    let paths: BTreeSet<_> = base
        .keys()
        .chain(ours.keys())
        .chain(theirs.keys())
        .collect();
    let mut out = Entries::new();
    let mut budget = MAX_TOTAL;
    for path in paths {
        let (a, b, c) = (
            base.get(path).copied(),
            ours.get(path).copied(),
            theirs.get(path).copied(),
        );
        let result = if b == c || c == a {
            b
        } else if b == a {
            c
        } else {
            let (a, b, c) = a
                .zip(b)
                .zip(c)
                .map(|((a, b), c)| (a, b, c))
                .context("merge conflict (add/delete); working tree unchanged")?;
            ensure!(
                [a.mode, b.mode, c.mode]
                    .iter()
                    .all(|m| matches!(m, 0o100644 | 0o100755)),
                "merge conflict in link or submodule; working tree unchanged"
            );
            let mode = if b.mode == c.mode || c.mode == a.mode {
                b.mode
            } else if b.mode == a.mode {
                c.mode
            } else {
                anyhow::bail!("merge conflict in file mode")
            };
            let a = blob(store, a, &mut budget)?;
            let b = blob(store, b, &mut budget)?;
            let c = blob(store, c, &mut budget)?;
            ensure!(
                !a.contains(&0) && !b.contains(&0) && !c.contains(&0),
                "binary merge conflict; working tree unchanged"
            );
            crate::compat::diff_budget(&a, &b)?;
            crate::compat::diff_budget(&a, &c)?;
            let data = diffy::merge_bytes(&a, &b, &c).map_err(|_| {
                anyhow::anyhow!(
                    "merge conflict at {:?}; working tree unchanged",
                    String::from_utf8_lossy(path)
                )
            })?;
            let object = Object {
                kind: Kind::Blob,
                data,
            };
            let oid = object.oid()?;
            repo.write_object(&object)?;
            Some(Entry { mode, oid })
        };
        if let Some(entry) = result {
            out.insert(path.clone(), entry);
        }
    }
    index::no_prefix_conflicts(&out)?;
    Ok(out)
}
fn clean(repo: &Repo, store: &mut Store<'_>, old: Oid) -> Result<()> {
    let entries = index(repo)?;
    ensure!(
        entries == tree(store, old)?,
        "staged changes would be overwritten"
    );
    let work = checkout::Work::new(repo)?;
    for (path, entry) in entries {
        if entry.mode != 0o160000 {
            ensure!(
                work.state(&path)? == Some(entry),
                "local changes would be overwritten"
            );
        }
    }
    Ok(())
}
pub fn review_merge(repo: &Repo, store: &mut Store<'_>, incoming: Oid, quiet: bool) -> Result<()> {
    let head = repo.head()?;
    let old = repo.read_ref(&head)?.context("unborn HEAD")?;
    clean(repo, store, old)?;
    if crate::object::ancestor(store, incoming, old)? {
        if !quiet {
            println!("Already up to date.");
        }
        return Ok(());
    }
    let base = revision::merge_base(store, old, incoming)?;
    let (a, b, c) = (
        tree(store, base)?,
        tree(store, old)?,
        tree(store, incoming)?,
    );
    let after = three_way(repo, store, &a, &b, &c)?;
    checkout::apply(
        repo,
        store,
        Update {
            reference: &head,
            old: Some(old),
            new: old,
            after,
            force: false,
            head_after: None,
            reset: false,
        },
    )?;
    repo.atomic("MERGE_HEAD", format!("{incoming}\n").as_bytes())?;
    if !quiet {
        eprintln!("Automatic merge went well; stopped before committing as requested");
    }
    Ok(())
}
pub fn rebase(
    repo: &Repo,
    store: &mut Store<'_>,
    incoming: Oid,
    overrides: &[String],
    quiet: bool,
) -> Result<()> {
    let head = repo.head()?;
    ensure!(head != "HEAD", "rebase requires a branch");
    let old = repo.read_ref(&head)?.context("unborn HEAD")?;
    clean(repo, store, old)?;
    if crate::object::ancestor(store, incoming, old)? {
        if !quiet {
            println!(
                "Current branch {} is up to date.",
                head.trim_start_matches("refs/heads/")
            );
        }
        return Ok(());
    }
    let base = revision::merge_base(store, old, incoming)?;
    let mut replay = Vec::new();
    let mut cursor = old;
    while cursor != base {
        ensure!(
            replay.len() < crate::object::MAX_OBJECTS,
            "rebase history exceeds limit"
        );
        let parents = store.get(cursor)?.commit()?.1;
        ensure!(
            parents.len() == 1,
            "rebasing local merge commits is unsupported; working tree unchanged"
        );
        replay.push(cursor);
        cursor = parents[0];
    }
    let mut current = incoming;
    let committer = if replay.is_empty() {
        None
    } else {
        Some(identity(repo, overrides, "COMMITTER")?)
    };
    for id in replay.into_iter().rev() {
        let object = store.get(id)?;
        let parent = object.commit()?.1[0];
        let (a, b, c) = (
            tree(store, parent)?,
            tree(store, current)?,
            tree(store, id)?,
        );
        let after = three_way(repo, store, &a, &b, &c)?;
        let tree = write_tree(repo, &after)?;
        if tree == store.get(current)?.commit()?.0 {
            continue;
        }
        let end = object
            .data
            .windows(2)
            .position(|w| w == b"\n\n")
            .context("invalid commit")?;
        let author = object.data[..end]
            .split(|b| *b == b'\n')
            .find_map(|l| l.strip_prefix(b"author "))
            .context("missing author")?;
        current = new_commit(
            repo,
            tree,
            &[current],
            std::str::from_utf8(author)?,
            committer.as_deref().unwrap(),
            &object.data[end + 2..],
        )?;
    }
    let after = tree(store, current)?;
    checkout::apply(
        repo,
        store,
        Update {
            reference: &head,
            old: Some(old),
            new: current,
            after,
            force: false,
            head_after: None,
            reset: false,
        },
    )?;
    if !quiet {
        println!("Successfully rebased and updated {head}.");
    }
    Ok(())
}
pub fn subject(data: &[u8]) -> String {
    let message = data
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|i| &data[i + 2..])
        .unwrap_or_default();
    crate::compat::text(&String::from_utf8_lossy(
        message.split(|b| *b == b'\n').next().unwrap_or_default(),
    ))
}
