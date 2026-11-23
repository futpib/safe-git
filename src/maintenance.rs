//! Conservative in-process object verification and packing. No hooks or prune commands.
use crate::{
    object::{self, ReadObjects},
    repo::{Repo, Store},
};
use anyhow::{Result, ensure};

pub fn run(
    repo: &Repo,
    store: &mut Store<'_>,
    command: &str,
    args: &[String],
    quiet: bool,
) -> Result<()> {
    for arg in args {
        ensure!(
            matches!(
                arg.as_str(),
                "--quiet" | "--full" | "--strict" | "--no-progress"
            ) || command == "gc"
                && matches!(arg.as_str(), "--auto" | "--prune=never" | "--prune=now"),
            "unsupported {command} option {arg}"
        );
    }
    let prune = command == "gc" && args.iter().any(|a| a == "--prune=now");
    ensure!(
        !prune || repo.common.is_none(),
        "run pruning from the main worktree"
    );
    let linked = if prune {
        crate::linked::protected_worktrees(repo)?
    } else {
        Vec::new()
    };
    let mut roots = if prune { protected(repo)? } else { Vec::new() };
    for other in &linked {
        roots.extend(protected(other)?);
    }
    let roots_before = roots.clone();
    store.allow_promisor = false;
    let ids = store.ids()?;
    let mut total = 0usize;
    for id in &ids {
        let obj = store.get(*id)?;
        total = total
            .checked_add(obj.data.len())
            .ok_or_else(|| anyhow::anyhow!("object size overflow"))?;
        ensure!(
            total <= object::MAX_TOTAL,
            "verification byte budget exceeded"
        );
        for (child, expected) in obj.children_for(repo.format)? {
            if obj.kind == object::Kind::Commit
                && repo.shallow.contains(id)
                && expected == object::Kind::Commit
            {
                continue;
            }
            ensure!(
                store.get(child)?.kind == expected,
                "object {id} has a child with an incorrect type: {child}"
            );
        }
    }
    for (name, id) in repo.refs()? {
        let obj = store.get(id)?;
        if name.starts_with("refs/heads/") {
            ensure!(
                obj.kind == object::Kind::Commit,
                "branch {name} does not point to a commit"
            );
        }
    }
    if let Some(head) = repo.read_ref("HEAD")? {
        ensure!(
            store.get(head)?.kind == object::Kind::Commit,
            "HEAD does not point to a commit"
        );
    }
    if repo.worktree.is_some() {
        let (entries, conflicts) = crate::porcelain::index_full(repo)?;
        for e in entries
            .values()
            .chain(conflicts.values().flat_map(|v| v.iter().flatten()))
        {
            if e.mode != 0o160000 && e.flags & crate::index::INTENT == 0 {
                ensure!(
                    store.get(e.oid)?.kind == object::Kind::Blob,
                    "index entry is not a blob"
                );
            }
        }
    }
    if command == "gc" && !ids.is_empty() {
        // Default gc retains history. Explicit pruning traces every protected
        // root and publishes a verified pack before removing old objects.
        let keep: std::collections::BTreeSet<_> = if prune {
            object::reachable_many_since(store, &roots, &std::collections::HashSet::new())?
                .into_iter()
                .collect()
        } else {
            ids.clone()
        };
        let old_packs = if prune {
            files(repo, "objects/pack")?
        } else {
            Vec::new()
        };
        ensure!(
            !old_packs
                .iter()
                .any(|p| p.ends_with(".keep") || p.ends_with(".promisor")),
            "kept/promisor packs prevent pruning"
        );
        let objects = keep
            .iter()
            .map(|id| store.get(*id))
            .collect::<Result<Vec<_>>>()?;
        let (pack, index) = crate::pack::encode_indexed_for(&objects, true, repo.format)?;
        crate::packed::Packed::new_for(pack.clone(), &index, repo.format)?;
        let name = hex::encode(&pack[pack.len() - repo.format.bytes()..]);
        repo.atomic(&format!("objects/pack/pack-{name}.idx"), &index)?;
        repo.atomic(&format!("objects/pack/pack-{name}.pack"), &pack)?;
        if prune {
            let mut current = protected(repo)?;
            for other in &linked {
                current.extend(protected(other)?);
            }
            ensure!(
                current == roots_before,
                "protected history changed during gc; old objects retained"
            );
        }
        for id in &ids {
            // Live objects are already in the durable verified pack. Unreachable
            // objects are removed only for the explicit --prune=now request.
            let text = id.to_string();
            let path = format!("objects/{}/{}", &text[..2], &text[2..]);
            if repo
                .read(&path, object::MAX_OBJECT + 1024 * 1024)?
                .is_some()
            {
                repo.remove(&path)?;
            }
        }
        for path in old_packs {
            let leaf = path.strip_prefix("objects/pack/pack-").unwrap_or("");
            if let Some(hash) = leaf
                .strip_suffix(".idx")
                .or_else(|| leaf.strip_suffix(".pack"))
            {
                ensure!(
                    hash.len() == repo.format.hex_len()
                        && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                    "invalid pack filename"
                );
                if hash != name {
                    repo.read(&path, object::MAX_PACK)?;
                    repo.remove(&path)?;
                }
            }
        }
    }
    if !quiet && !args.iter().any(|s| s == "--quiet") {
        println!(
            "Verified {} objects{}",
            ids.len(),
            if command == "gc" {
                if prune {
                    "; packed reachable objects and pruned unreachable history"
                } else {
                    "; packed local objects without expiring history"
                }
            } else {
                ""
            }
        );
    }
    Ok(())
}

// Discover metadata through directory descriptors; never follow a repository
// symlink while collecting roots or expiring caller-selected reflogs.
pub(crate) fn files(repo: &Repo, prefix: &str) -> Result<Vec<String>> {
    use rustix::fs::{self, AtFlags, Mode, OFlags};
    use std::fs::File;
    let (root, _) = match repo.parent(&format!("{prefix}/.scan"), false) {
        Ok(v) => v,
        Err(e) if e.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT) => {
            return Ok(Vec::new());
        }
        Err(e) => return Err(e),
    };
    fn walk(
        dir: &File,
        prefix: &str,
        out: &mut Vec<String>,
        depth: usize,
        count: &mut usize,
    ) -> Result<()> {
        ensure!(depth <= 128, "metadata depth exceeds limit");

        for entry in fs::Dir::read_from(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_str()?;
            if matches!(name, "." | "..") {
                continue;
            }
            *count += 1;
            ensure!(
                *count <= object::MAX_OBJECTS && out.len() < object::MAX_OBJECTS,
                "metadata scan exceeds limit"
            );
            let stat = fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
            let path = format!("{prefix}/{name}");
            if stat.st_mode & 0o170000 == 0o40000 {
                let child = File::from(fs::openat(
                    dir,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?);
                walk(&child, &path, out, depth + 1, count)?;
            } else {
                ensure!(
                    stat.st_mode & 0o170000 == 0o100000 && stat.st_nlink == 1,
                    "unsafe metadata file"
                );
                out.push(path);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(&root, prefix, &mut out, 0, &mut 0)?;
    out.sort();
    Ok(out)
}
fn protected(repo: &Repo) -> Result<Vec<object::Oid>> {
    for state in [
        "safe-git-checkout",
        "safe-git-sequence",
        "safe-git-am",
        "safe-git-autostash",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-apply",
        "rebase-merge",
        "BISECT_START",
        "safe-git-bisect",
        "index.lock",
        "HEAD.lock",
        "packed-refs.lock",
    ] {
        ensure!(
            !repo.exists(state)?,
            "finish or abort the active operation before pruning"
        );
    }
    let mut ids: Vec<_> = repo.refs()?.into_values().collect();
    ids.extend(repo.read_ref("HEAD")?);
    for name in ["ORIG_HEAD", "FETCH_HEAD"] {
        if let Some(bytes) = repo.read(name, 8 * 1024 * 1024)? {
            for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
                let word = line.split(|b| b.is_ascii_whitespace()).next().unwrap();
                ids.push(object::Oid::parse(word)?);
            }
        }
    }
    for path in files(repo, "logs")? {
        let bytes = repo.read(&path, 8 * 1024 * 1024)?.unwrap_or_default();
        for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            let fields: Vec<_> = line.split(|b| *b == b' ').take(2).collect();
            ensure!(fields.len() == 2, "invalid reflog");
            for field in fields {
                let id = object::Oid::parse(field)?;
                if !id.is_zero() {
                    ids.push(id);
                }
            }
            ensure!(
                ids.len() <= object::MAX_OBJECTS,
                "too many protected objects"
            );
        }
    }
    if repo.worktree.is_some() {
        let (entries, conflicts) = crate::porcelain::index_full(repo)?;
        ids.extend(
            entries
                .values()
                .chain(conflicts.values().flat_map(|s| s.iter().flatten()))
                .filter(|e| e.mode != 0o160000 && e.flags & crate::index::INTENT == 0)
                .map(|e| e.oid),
        );
    }
    ids.extend(crate::rerere::roots(repo)?);
    Ok(ids)
}
pub fn expire_reflogs(repo: &Repo, args: &[String]) -> Result<()> {
    use anyhow::Context;
    let mut all = false;
    let mut now = false;
    let mut refs = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--all" => all = true,
            "--expire=now" | "--expire=all" => now = true,
            "--expire-unreachable=now" => {}
            a if a.starts_with('-') => anyhow::bail!("unsupported reflog expiry option {a}"),
            _ => refs.push(arg),
        }
    }
    ensure!(
        now && (all || !refs.is_empty()),
        "explicit --expire=now and --all or refs required"
    );
    let paths = if all {
        files(repo, "logs")?
    } else {
        refs.into_iter()
            .map(|r| {
                let name = if r == "HEAD" || r.starts_with("refs/") {
                    r.clone()
                } else {
                    format!("refs/heads/{r}")
                };
                crate::repo::reference(&name)?;
                Ok(format!("logs/{name}"))
            })
            .collect::<Result<Vec<_>>>()?
    };
    for path in paths {
        let original = repo.read(&path, 8 * 1024 * 1024)?;
        let (dir, name) = repo.parent(&path, false)?;
        let mut lock =
            crate::repo::Pending::new(dir, format!("{name}.lock")).context("lock reflog")?;
        ensure!(
            repo.read(&path, 8 * 1024 * 1024)? == original,
            "reflog changed concurrently"
        );
        lock.commit(&name)?;
    }
    Ok(())
}
