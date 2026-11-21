//! Inert worktree updates. No attributes, filters, submodule checkout, or hooks.
use crate::{
    index::{self, Entries, Entry},
    object::{self, Kind, MAX_OBJECT, MAX_OBJECTS, MAX_TOTAL, Object, Oid, ReadObjects},
    repo::{Pending, Repo, Store},
};
use anyhow::{Context, Result, bail, ensure};
use rustix::{
    fs::{self, AtFlags, Mode, OFlags, RenameFlags},
    io::Errno,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    sync::Arc,
};

const TXN: &str = "safe-git-checkout";
const DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW);
const INDEX_LIMIT: usize = 64 * 1024 * 1024;

struct Work<'a> {
    root: &'a File,
    device: u64,
}
impl<'a> Work<'a> {
    fn new(repo: &'a Repo) -> Result<Self> {
        let root = repo
            .worktree
            .as_ref()
            .context("command requires a working tree")?;
        let device = root.metadata()?.dev();
        ensure!(
            repo.root.metadata()?.dev() == device,
            "worktree and .git must be on the same filesystem"
        );
        Ok(Self { root, device })
    }
    fn parent(&self, path: &[u8]) -> Result<(File, Vec<u8>)> {
        index::path(path)?;
        let mut components: Vec<_> = path.split(|b| *b == b'/').collect();
        let name = components.pop().context("empty path")?.to_vec();
        let mut dir = self.root.try_clone()?;
        for component in components {
            dir = File::from(
                fs::openat(&dir, component, DIR, Mode::empty())
                    .context("worktree parent is missing, a symlink, or not a directory")?,
            );
            ensure!(
                dir.metadata()?.dev() == self.device,
                "cross-filesystem worktree traversal is unsupported"
            );
        }
        Ok((dir, name))
    }
    fn state(&self, path: &[u8]) -> Result<Option<Entry>> {
        let (dir, name) = match self.parent(path) {
            Ok(v) => v,
            Err(e) if missing(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        entry_at(&dir, &name, self.device)
    }
    fn plan_directories(
        &self,
        path: &[u8],
        include_leaf: bool,
        dirs: &mut BTreeSet<Vec<u8>>,
    ) -> Result<()> {
        let mut ends: Vec<_> = path
            .iter()
            .enumerate()
            .filter(|(_, b)| **b == b'/')
            .map(|(i, _)| i)
            .collect();
        if include_leaf {
            ends.push(path.len());
        }
        for end in ends {
            let prefix = &path[..end];
            match self.state(prefix)? {
                None => {
                    dirs.insert(prefix.to_vec());
                }
                Some(e) => ensure!(
                    e.mode == 0o40000,
                    "file/directory transitions are unsupported; no files changed"
                ),
            }
        }
        Ok(())
    }
}
fn missing(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Errno>() == Some(&Errno::NOENT)
}
fn entry_at(dir: &File, name: &[u8], device: u64) -> Result<Option<Entry>> {
    let stat = match fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(s) => s,
        Err(Errno::NOENT) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        stat.st_dev == device,
        "cross-filesystem worktree entry is unsupported"
    );
    let kind = stat.st_mode & 0o170000;
    if kind == 0o40000 {
        return Ok(Some(Entry {
            mode: 0o40000,
            oid: Oid::ZERO,
        }));
    }
    let (mode, data) = match kind {
        0o100000 => {
            let file = File::from(fs::openat(
                dir,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
            let meta = file.metadata()?;
            ensure!(
                meta.is_file() && meta.dev() == device,
                "worktree file changed type"
            );
            let mut bytes = Vec::new();
            file.take(MAX_OBJECT as u64 + 1).read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= MAX_OBJECT, "worktree file exceeds limit");
            (
                if meta.mode() & 0o111 == 0 {
                    0o100644
                } else {
                    0o100755
                },
                bytes,
            )
        }
        0o120000 => {
            let target = fs::readlinkat(dir, name, Vec::new())?;
            ensure!(
                target.as_bytes().len() <= 4096,
                "symlink target exceeds limit"
            );
            (0o120000, target.into_bytes())
        }
        _ => bail!("special worktree files are unsupported"),
    };
    Ok(Some(Entry {
        mode,
        oid: Object {
            kind: Kind::Blob,
            data,
        }
        .oid()?,
    }))
}
fn symlink_target(path: &[u8], target: &[u8]) -> Result<()> {
    ensure!(
        !target.is_empty()
            && target.len() <= 4096
            && !target.starts_with(b"/")
            && !target.contains(&0),
        "absolute, empty, or invalid symlink target"
    );
    let mut parts: Vec<Vec<u8>> = path.split(|b| *b == b'/').map(Vec::from).collect();
    parts.pop();
    for part in target.split(|b| *b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                ensure!(
                    parts.pop().is_some(),
                    "symlink would escape the working tree"
                );
            }
            _ => {
                index::path(part)?;
                parts.push(part.to_vec());
            }
        }
    }
    // No subsequent access follows a symlink, including a symlink to another symlink.
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Change {
    path: Vec<u8>,
    old: Option<Entry>,
    new: Option<Entry>,
}
#[derive(Serialize, Deserialize)]
struct Journal {
    version: u8,
    head: String,
    old: Option<Oid>,
    new: Oid,
    index_old: Option<Oid>,
    index_new: Oid,
    index_lock: u64,
    ref_lock: u64,
    changes: Vec<Change>,
    dirs: Vec<Vec<u8>>,
}
impl Journal {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1
                && self.changes.len() <= MAX_OBJECTS
                && self.dirs.len() <= MAX_OBJECTS,
            "invalid checkout journal"
        );
        crate::repo::refname(&self.head)?;
        ensure!(
            self.head.starts_with("refs/heads/") && self.new != Oid::ZERO,
            "invalid journal branch"
        );
        let mut names = BTreeSet::new();
        for change in &self.changes {
            index::path(&change.path)?;
            ensure!(names.insert(change.path.clone()), "duplicate journal path");
            for e in [change.old, change.new].into_iter().flatten() {
                index::mode(e.mode)?;
                ensure!(e.mode != 0o160000, "gitlink in file journal");
            }
        }
        for dir in &self.dirs {
            index::path(dir)?;
        }
        Ok(())
    }
}

pub fn merge(
    repo: &Repo,
    store: &mut Store<'_>,
    head: &str,
    old: Option<Oid>,
    new: Oid,
) -> Result<()> {
    ensure!(
        !repo.exists(TXN)?,
        "interrupted checkout; run safe-git recover"
    );
    for state in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-apply",
        "rebase-merge",
    ] {
        ensure!(!repo.exists(state)?, "unfinished Git operation: {state}");
    }
    let work = Work::new(repo)?;
    let ref_update = repo.prepare_ref(head, old)?;
    let mut index_lock = Pending::new(repo.root.try_clone()?, "index.lock".into())
        .context("acquire Git index lock")?;
    let original = repo.read("index", INDEX_LIMIT)?;
    let mut index = match &original {
        Some(bytes) => index::decode(bytes)?,
        None => Entries::new(),
    };
    let before = index::tree(store, old)?;
    let after = index::tree(store, Some(new))?;
    let keys: BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
    let mut changes = Vec::new();
    let mut contents = Vec::<Option<Arc<Object>>>::new();
    let mut dirs = BTreeSet::new();
    let mut total = 0;
    for path in keys {
        let a = before.get(&path).copied();
        let b = after.get(&path).copied();
        let staged = index.get(&path).copied();
        if a == b || staged == b {
            continue;
        }
        ensure!(
            staged == a,
            "staged changes would be overwritten at {:?}",
            String::from_utf8_lossy(&path)
        );
        if a.is_some_and(|e| e.mode == 0o160000) || b.is_some_and(|e| e.mode == 0o160000) {
            ensure!(
                a.is_none_or(|e| e.mode == 0o160000) && b.is_none_or(|e| e.mode == 0o160000),
                "submodule/file transitions are unsupported"
            );
            if a.is_none() {
                work.plan_directories(&path, true, &mut dirs)?;
            }
            // Gitlink updates affect only the index. Existing submodule contents are untouched.
        } else {
            let actual = work.state(&path)?;
            ensure!(
                actual == a || (actual.is_none() && b.is_none()),
                "local changes or untracked data would be overwritten at {:?}",
                String::from_utf8_lossy(&path)
            );
            if actual.is_some() || b.is_some() {
                let content = if let Some(entry) = b {
                    work.plan_directories(&path, false, &mut dirs)?;
                    let object = store.get(entry.oid)?;
                    ensure!(object.kind == Kind::Blob, "checkout entry is not a blob");
                    total += object.data.len();
                    ensure!(total <= MAX_TOTAL, "checkout byte budget exceeded");
                    if entry.mode == 0o120000 {
                        symlink_target(&path, &object.data)?;
                    }
                    Some(object)
                } else {
                    None
                };
                changes.push(Change {
                    path: path.clone(),
                    old: actual,
                    new: b,
                });
                contents.push(content);
            }
        }
        if let Some(entry) = b {
            index.insert(path, entry);
        } else {
            index.remove(&path);
        }
    }
    index::no_prefix_conflicts(&index)?;
    let new_index = index::encode(&index)?;
    let journal = Journal {
        version: 1,
        head: head.into(),
        old,
        new,
        index_old: original.as_ref().map(|b| object::hash(&[b])).transpose()?,
        index_new: object::hash(&[&new_index])?,
        index_lock: index_lock.file.metadata()?.ino(),
        ref_lock: ref_update.lock.file.metadata()?.ino(),
        changes,
        dirs: dirs.into_iter().collect(),
    };
    journal.validate()?;
    let journal_bytes = serde_json::to_vec(&journal)?;
    ensure!(
        journal_bytes.len() <= INDEX_LIMIT,
        "checkout journal exceeds limit"
    );
    fs::mkdirat(&repo.root, TXN, Mode::RUSR | Mode::WUSR | Mode::XUSR)?;
    repo.root.sync_all()?;
    let txn = File::from(fs::openat(&repo.root, TXN, DIR, Mode::empty())?);
    // All new content and the old index are durable before touching the working tree.
    let staging = (|| -> Result<()> {
        if let Some(bytes) = &original {
            write_new(&txn, "index-old", bytes, 0o600)?;
        }
        for (i, (change, object)) in journal.changes.iter().zip(contents.iter()).enumerate() {
            if let (Some(entry), Some(object)) = (change.new, object) {
                let name = format!("new-{i}");
                if entry.mode == 0o120000 {
                    fs::symlinkat(object.data.as_slice(), &txn, name.as_str())?;
                } else {
                    write_new(&txn, &name, &object.data, entry.mode & 0o777)?;
                }
            }
        }
        write_new(&txn, "journal", &journal_bytes, 0o600)?;
        txn.sync_all()?;
        Ok(())
    })();
    if let Err(error) = staging {
        cleanup(repo, &txn, &journal)?;
        return Err(error);
    }
    let apply = (|| -> Result<()> {
        for path in &journal.dirs {
            let (dir, name) = work.parent(path)?;
            fs::mkdirat(&dir, name.as_slice(), Mode::from_raw_mode(0o755))?;
            dir.sync_all()?;
        }
        for (i, change) in journal.changes.iter().enumerate() {
            ensure!(
                work.state(&change.path)? == change.old,
                "worktree changed during checkout"
            );
            let (dir, name) = work.parent(&change.path)?;
            if change.old.is_some() {
                fs::renameat_with(
                    &dir,
                    name.as_slice(),
                    &txn,
                    format!("old-{i}"),
                    RenameFlags::NOREPLACE,
                )?;
                dir.sync_all()?;
                txn.sync_all()?;
            }
            if change.new.is_some() {
                fs::renameat_with(
                    &txn,
                    format!("new-{i}"),
                    &dir,
                    name.as_slice(),
                    RenameFlags::NOREPLACE,
                )?;
                dir.sync_all()?;
                txn.sync_all()?;
            }
        }
        ensure!(repo.head()? == head, "HEAD changed during checkout");
        index_lock.file.write_all(&new_index)?;
        index_lock.commit("index")?;
        ref_update.commit(new)?;
        Ok(())
    })();
    drop(index_lock);
    match apply {
        Ok(()) => cleanup(repo, &txn, &journal),
        Err(error) => {
            // RefUpdate is dropped when the closure returns, releasing its lock.
            recover(repo).with_context(||format!("checkout failed ({error:#}); recovery also failed; backups remain in .git/{TXN}"))?;
            Err(error)
        }
    }
}
fn write_new(dir: &File, name: &str, bytes: &[u8], mode: u32) -> Result<()> {
    let mut file = File::from(fs::openat(
        dir,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(mode),
    )?);
    file.write_all(bytes)?;
    fs::fchmod(&file, Mode::from_raw_mode(mode))?;
    file.sync_all()?;
    Ok(())
}
fn exists_at(dir: &File, name: &str) -> Result<bool> {
    match fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(true),
        Err(Errno::NOENT) => Ok(false),
        Err(e) => Err(e.into()),
    }
}
fn unlink_if_present(dir: &File, name: &str) -> Result<()> {
    match fs::unlinkat(dir, name, AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
fn cleanup(repo: &Repo, txn: &File, journal: &Journal) -> Result<()> {
    for i in 0..journal.changes.len() {
        unlink_if_present(txn, &format!("new-{i}"))?;
        unlink_if_present(txn, &format!("old-{i}"))?;
    }
    unlink_if_present(txn, "index-old")?;
    unlink_if_present(txn, "journal")?;
    txn.sync_all()?;
    fs::unlinkat(&repo.root, TXN, AtFlags::REMOVEDIR)?;
    repo.root.sync_all()?;
    Ok(())
}
fn release_our_lock(repo: &Repo, path: &str, inode: u64) -> Result<()> {
    let (dir, name) = repo.parent(path, false)?;
    match fs::statat(&dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => {
            ensure!(
                stat.st_ino == inode && stat.st_mode & 0o170000 == 0o100000,
                "another operation owns {path}"
            );
            fs::unlinkat(&dir, name.as_str(), AtFlags::empty())?;
        }
        Err(Errno::NOENT) => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
pub fn recover(repo: &Repo) -> Result<()> {
    let work = Work::new(repo)?;
    let bytes = repo
        .read(&format!("{TXN}/journal"), INDEX_LIMIT)?
        .context("no complete checkout journal; no automatic recovery is possible")?;
    let journal: Journal = serde_json::from_slice(&bytes)?;
    journal.validate()?;
    let txn = File::from(fs::openat(&repo.root, TXN, DIR, Mode::empty())?);
    ensure!(
        repo.head()? == journal.head,
        "HEAD differs from interrupted checkout; preserve backups and recover manually"
    );
    let tip = repo.read_ref(&journal.head)?;
    ensure!(
        tip == journal.old || tip == Some(journal.new),
        "branch changed since interrupted checkout"
    );
    release_our_lock(repo, "index.lock", journal.index_lock)?;
    release_our_lock(repo, &format!("{}.lock", journal.head), journal.ref_lock)?;
    let _branch_lock = repo.prepare_ref(&journal.head, tip)?;
    let mut index_lock = Pending::new(repo.root.try_clone()?, "index.lock".into())?;
    if tip == Some(journal.new) {
        // The ref was committed last: all file/index changes finished. Leave any
        // subsequent user edits alone and only remove our backup files.
        return cleanup(repo, &txn, &journal);
    }
    let old_index = repo.read(&format!("{TXN}/index-old"), INDEX_LIMIT)?;
    ensure!(
        old_index.as_ref().map(|b| object::hash(&[b])).transpose()? == journal.index_old,
        "old index backup checksum mismatch"
    );
    if let Some(bytes) = &old_index {
        index::decode(bytes)?;
    }
    let current_index = repo.read("index", INDEX_LIMIT)?;
    let current_hash = current_index
        .as_ref()
        .map(|b| object::hash(&[b]))
        .transpose()?;
    ensure!(
        current_hash == journal.index_old || current_hash == Some(journal.index_new),
        "index changed since interrupted checkout"
    );
    for (i, change) in journal.changes.iter().enumerate().rev() {
        let backup = format!("old-{i}");
        let staged = format!("new-{i}");
        let has_backup = exists_at(&txn, &backup)?;
        let has_staged = exists_at(&txn, &staged)?;
        let actual = work.state(&change.path)?;
        if has_backup {
            ensure!(
                entry_at(&txn, backup.as_bytes(), work.device)? == change.old,
                "worktree backup does not match journal"
            );
            let (dir, name) = work.parent(&change.path)?;
            if actual.is_some() {
                ensure!(
                    !has_staged && actual == change.new,
                    "new user edits block recovery; backup retained"
                );
                fs::unlinkat(&dir, name.as_slice(), AtFlags::empty())?;
            }
            fs::renameat_with(
                &txn,
                backup.as_str(),
                &dir,
                name.as_slice(),
                RenameFlags::NOREPLACE,
            )?;
            dir.sync_all()?;
            txn.sync_all()?;
        } else if change.old.is_none() && !has_staged && actual.is_some() {
            ensure!(actual == change.new, "new user edits block recovery");
            let (dir, name) = work.parent(&change.path)?;
            fs::unlinkat(&dir, name.as_slice(), AtFlags::empty())?;
            dir.sync_all()?;
        } else if change.old.is_some() {
            ensure!(
                actual == change.old,
                "original file changed or its backup is missing; recover manually"
            );
        }
    }
    for path in journal.dirs.iter().rev() {
        if work.state(path)?.is_some() {
            let (dir, name) = work.parent(path)?;
            match fs::unlinkat(&dir, name.as_slice(), AtFlags::REMOVEDIR) {
                Ok(()) => dir.sync_all()?,
                Err(Errno::NOTEMPTY) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    if let Some(bytes) = old_index {
        index_lock.file.write_all(&bytes)?;
        index_lock.commit("index")?;
    } else {
        unlink_if_present(&repo.root, "index")?;
        repo.root.sync_all()?;
    }
    cleanup(repo, &txn, &journal)
}
