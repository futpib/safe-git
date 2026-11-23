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
            ) || command == "gc" && matches!(arg.as_str(), "--auto" | "--prune=never"),
            "unsupported {command} option {arg}"
        );
    }
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
        for (child, expected) in obj.children()? {
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
            if e.mode != 0o160000 {
                ensure!(
                    store.get(e.oid)?.kind == object::Kind::Blob,
                    "index entry is not a blob"
                );
            }
        }
    }
    if command == "gc" && !ids.is_empty() {
        // Include unreachable objects too: reflogs, interrupted operations and other
        // clients may still need them. Publish a verified pack before removing duplicates.
        let objects = ids
            .iter()
            .map(|id| store.get(*id))
            .collect::<Result<Vec<_>>>()?;
        let (pack, index) = crate::pack::encode_indexed(&objects, true)?;
        crate::packed::Packed::new(pack.clone(), &index)?;
        let name = hex::encode(&pack[pack.len() - 20..]);
        repo.atomic(&format!("objects/pack/pack-{name}.idx"), &index)?;
        repo.atomic(&format!("objects/pack/pack-{name}.pack"), &pack)?;
        // Keep pre-existing packs: live Git readers may have their indexes open.
        for id in &ids {
            let name = id.to_string();
            let path = format!("objects/{}/{}", &name[..2], &name[2..]);
            // read() rejects symlinks, hard links and non-regular files before unlink.
            if repo
                .read(&path, object::MAX_OBJECT + 1024 * 1024)?
                .is_some()
            {
                repo.remove(&path)?;
            }
        }
    }
    if !quiet && !args.iter().any(|s| s == "--quiet") {
        println!(
            "Verified {} objects{}",
            ids.len(),
            if command == "gc" {
                "; packed local objects without expiring history"
            } else {
                ""
            }
        );
    }
    Ok(())
}
