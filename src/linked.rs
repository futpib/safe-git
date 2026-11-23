//! Linked metadata is followed only through a host-owned, inode-bound registration.
use crate::{
    Cli, checkout, porcelain,
    repo::{Repo, Store},
    revision, trust,
};
use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{self, AtFlags, Mode, OFlags};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Write},
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::{Path, PathBuf},
};
const DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
#[derive(Clone, Serialize, Deserialize)]
pub struct Identity {
    pub path: PathBuf,
    dev: u64,
    ino: u64,
}
impl Identity {
    pub fn of(file: &File) -> Result<Self> {
        let stat = file.metadata()?;
        Ok(Self {
            path: location(file)?,
            dev: stat.dev(),
            ino: stat.ino(),
        })
    }
    pub fn matches(&self, file: &File) -> Result<bool> {
        let stat = file.metadata()?;
        Ok(self.path == location(file)? && self.dev == stat.dev() && self.ino == stat.ino())
    }
    pub fn open(&self) -> Result<File> {
        let file = open_directory(&self.path)?;
        ensure!(
            self.matches(&file)?,
            "registered directory was replaced or moved"
        );
        Ok(file)
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Link {
    pub work: Identity,
    pub metadata: Identity,
    pub common: Identity,
}
pub fn location(file: &File) -> Result<PathBuf> {
    Ok(std::fs::read_link(format!(
        "/proc/self/fd/{}",
        file.as_raw_fd()
    ))?)
}
pub fn open_directory(path: &Path) -> Result<File> {
    ensure!(path.is_absolute(), "registered directory must be absolute");
    let mut dir = File::from(fs::open("/", DIR, Mode::empty())?);
    for part in path.components() {
        match part {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(p) => {
                dir = File::from(fs::openat(&dir, p, DIR, Mode::empty())?)
            }
            _ => bail!("invalid registered directory"),
        }
    }
    Ok(dir)
}
fn records() -> Result<Vec<Link>> {
    let path = trust::directory()?.join("worktrees.json");
    let file = match fs::open(
        &path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(f) => File::from(f),
        Err(rustix::io::Errno::NOENT) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let stat = file.metadata()?;
    ensure!(
        stat.is_file() && stat.nlink() == 1 && stat.mode() & 0o022 == 0,
        "unsafe worktree registry"
    );
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "worktree registry exceeds limit"
    );
    let records: Vec<Link> = serde_json::from_slice(&bytes)?;
    ensure!(records.len() <= 1024, "too many worktrees");
    Ok(records)
}
fn save(records: &[Link]) -> Result<()> {
    let dir = trust::directory()?;
    let mut file = tempfile::NamedTempFile::new_in(&dir)?;
    let bytes = serde_json::to_vec(records)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "worktree registry exceeds limit"
    );
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(dir.join("worktrees.json"))?;
    File::open(dir)?.sync_all()?;
    Ok(())
}
pub fn lookup(work: &File) -> Result<Link> {
    for link in records()? {
        if link.work.matches(work)? {
            let metadata = link.metadata.open()?;
            link.common.open()?;
            let file = File::from(fs::openat(
                work,
                ".git",
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
            ensure!(
                file.metadata()?.is_file() && file.metadata()?.nlink() == 1,
                "unsafe gitdir file"
            );
            let mut bytes = Vec::new();
            file.take(8193).read_to_end(&mut bytes)?;
            ensure!(
                bytes == format!("gitdir: {}\n", location(&metadata)?.display()).as_bytes(),
                "gitdir differs from host registration"
            );
            return Ok(link);
        }
    }
    bail!("linked worktree is not registered in trusted host state")
}
pub fn available(repo: &Repo, branch: &str) -> Result<()> {
    let common = repo.common.as_ref().unwrap_or(&repo.root);
    if repo.common.is_some()
        && !crate::config::get(repo, &[], "core.bare")?.is_some_and(|v| v == "true")
    {
        let head = read_head(common)?;
        ensure!(
            head.trim_end() != format!("ref: {branch}"),
            "branch is checked out in the main worktree"
        );
    }
    for link in records()? {
        if link.common.matches(common)? && !link.metadata.matches(&repo.root)? {
            // A missing registered worktree cannot authorize any path access or ref rewrite.
            let meta=link.metadata.open().context("registered worktree metadata changed; prune stale registrations before reusing branches")?;
            let head = read_head(&meta)?;
            ensure!(
                head.trim_end() != format!("ref: {branch}"),
                "branch is checked out in another worktree"
            );
        }
    }
    Ok(())
}
pub fn run(cli: &Cli, repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    let command = args.first().map(String::as_str).unwrap_or("list");
    let common = repo.common.as_ref().unwrap_or(&repo.root);
    let mut links = records()?;
    match command {
        "list" => {
            ensure!(
                args.len() <= 2 && args.get(1).is_none_or(|v| v == "--porcelain"),
                "unsupported worktree list option"
            );
            let root = location(common)?;
            let bare = crate::config::get(repo, &[], "core.bare")?.is_some_and(|v| v == "true");
            let main = if bare {
                root.as_path()
            } else {
                root.parent().context("metadata has no parent")?
            };
            let machine = args.get(1).is_some();
            list_entry(repo, main, common, machine, bare)?;
            for l in &links {
                if l.common.matches(common)? {
                    let metadata = l.metadata.open()?;
                    list_entry(repo, &l.work.path, &metadata, machine, false)?;
                }
            }
        }
        "add" => {
            let mut branch = None;
            let mut detach = false;
            let mut values = Vec::new();
            let mut items = args[1..].iter();
            while let Some(a) = items.next() {
                match a.as_str() {
                    "-b" => branch = Some(items.next().context("missing branch")?.as_str()),
                    "--detach" | "-d" => detach = true,
                    a if a.starts_with('-') => bail!("unsupported worktree add option {a}"),
                    _ => values.push(a.as_str()),
                }
            }
            ensure!(
                !values.is_empty() && values.len() <= 2,
                "worktree add requires PATH [REV]"
            );
            ensure!(
                !detach || branch.is_none(),
                "cannot create a detached branch"
            );
            let path = cli.repository.join(values[0]);
            let parent = path
                .parent()
                .context("missing worktree parent")?
                .canonicalize()?;
            let path = parent.join(path.file_name().context("invalid destination")?);
            ensure!(
                std::fs::symlink_metadata(&path).is_err(),
                "worktree destination exists"
            );
            let start = values.get(1).copied().unwrap_or("HEAD");
            let inferred = path
                .file_name()
                .context("invalid path")?
                .to_str()
                .context("non-UTF-8 branch")?;
            let name = if detach {
                None
            } else {
                match branch {
                    Some(name) => Some(name),
                    None if values.len() > 1 => {
                        let local = start.strip_prefix("refs/heads/").unwrap_or(start);
                        if crate::repo::branch(local).is_ok()
                            && repo.read_ref(&format!("refs/heads/{local}"))?.is_some()
                        {
                            Some(local)
                        } else {
                            None
                        }
                    }
                    None => Some(inferred),
                }
            };
            let reference = name
                .map(|n| crate::repo::branch(n).map(|n| format!("refs/heads/{n}")))
                .transpose()?;
            if let Some(r) = &reference {
                available(repo, r)?;
                ensure!(
                    repo.head()? != *r || repo.worktree.is_none(),
                    "branch already checked out"
                );
                ensure!(
                    branch.is_none() || repo.read_ref(r)?.is_none(),
                    "branch already exists"
                );
            }
            let id = if values.len() == 1 && branch.is_none() {
                match reference
                    .as_deref()
                    .map(|r| repo.read_ref(r))
                    .transpose()?
                    .flatten()
                {
                    Some(id) => id,
                    None => revision::commit(repo, store, start)?,
                }
            } else {
                revision::commit(repo, store, start)?
            };
            let stage = tempfile::Builder::new()
                .prefix(".safe-git-worktree-")
                .tempdir_in(&parent)?;
            let (meta_parent_fd, _) = repo.parent("worktrees/.check", true)?;
            let meta_parent =
                PathBuf::from(format!("/proc/self/fd/{}", meta_parent_fd.as_raw_fd()));
            let metadata = tempfile::Builder::new()
                .prefix("safe-")
                .tempdir_in(&meta_parent)?;
            let mut linked =
                Repo::lock(File::from(fs::open(metadata.path(), DIR, Mode::empty())?))?;
            linked.common = Some(common.try_clone()?);
            linked.shallow = crate::shallow::read(&linked)?;
            linked.worktree = Some(open_directory(stage.path())?);
            linked.atomic("HEAD", format!("{id}\n").as_bytes())?;
            let mut linked_store = Store::new(&linked);
            let head_after = reference.clone().unwrap_or(id.to_string());
            let target = reference.as_deref().unwrap_or("HEAD");
            let old = if target == "HEAD" {
                Some(id)
            } else {
                repo.read_ref(target)?
            };
            checkout::apply(
                &linked,
                &mut linked_store,
                checkout::Update {
                    reference: target,
                    old,
                    new: id,
                    after: porcelain::tree(store, id)?,
                    force: true,
                    head_after: Some(head_after),
                    reset: true,
                },
            )?;
            std::fs::write(
                stage.path().join(".git"),
                format!("gitdir: {}\n", location(&linked.root)?.display()),
            )?;
            linked.atomic("commondir", b"../..\n")?;
            linked.atomic(
                "gitdir",
                format!("{}\n", path.join(".git").display()).as_bytes(),
            )?;
            fs::renameat_with(
                fs::CWD,
                stage.path(),
                fs::CWD,
                &path,
                fs::RenameFlags::NOREPLACE,
            )?;
            let _ = stage.keep();
            let _ = metadata.keep();
            links.push(Link {
                work: Identity::of(linked.worktree.as_ref().unwrap())?,
                metadata: Identity::of(&linked.root)?,
                common: Identity::of(common)?,
            });
            save(&links)?;
        }
        "remove" => {
            let force = args.iter().any(|a| a == "-f" || a == "--force");
            let values: Vec<_> = args[1..].iter().filter(|a| !a.starts_with('-')).collect();
            ensure!(values.len() == 1, "worktree remove requires PATH");
            let path = cli.repository.join(values[0]).canonicalize()?;
            let at = links
                .iter()
                .position(|l| l.work.path == path)
                .context("worktree is not registered")?;
            let l = links[at].clone();
            ensure!(
                l.common.matches(common)? && !l.metadata.matches(&repo.root)?,
                "not another worktree of this repository"
            );
            let work = l.work.open()?;
            let meta = l.metadata.open()?;
            if !force {
                let mut other = Repo::lock(meta.try_clone()?)?;
                other.common = Some(common.try_clone()?);
                other.worktree = Some(work.try_clone()?);
                let index = porcelain::index(&other)?;
                let mut objects = Store::new(&other);
                ensure!(
                    index == index::tree(&mut objects, other.read_ref("HEAD")?)?
                        && checkout::snapshot(&other, &index)? == index,
                    "worktree has local changes"
                );
                ensure!(
                    crate::workflow::untracked(&other, &index, &[Vec::new()], true)?.is_empty(),
                    "worktree contains untracked files"
                );
            }
            remove_directory(&l.work)?;
            remove_directory(&l.metadata)?;
            links.remove(at);
            save(&links)?;
        }
        "prune" => {
            ensure!(args.len() == 1, "unsupported prune option");
            links.retain(|l| l.work.open().is_ok());
            save(&links)?;
        }
        _ => bail!("unsupported worktree command {command}"),
    }
    Ok(())
}
use crate::index;
pub fn remove_directory(identity: &Identity) -> Result<()> {
    let root = identity.open()?;
    let mut budget = crate::object::MAX_OBJECTS;
    fn clear(dir: &File, budget: &mut usize, depth: usize) -> Result<()> {
        ensure!(depth <= 128, "directory nesting limit exceeded");
        let device = dir.metadata()?.dev();
        for e in fs::Dir::read_from(dir)? {
            let e = e?;
            let name = e.file_name().to_bytes();
            if matches!(name, b"." | b"..") {
                continue;
            }
            *budget = budget
                .checked_sub(1)
                .context("directory deletion budget exceeded")?;
            let stat = fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
            ensure!(
                stat.st_dev == device,
                "refusing to cross a filesystem boundary"
            );
            if stat.st_mode & 0o170000 == 0o40000 {
                let child = File::from(fs::openat(dir, name, DIR, Mode::empty())?);
                clear(&child, budget, depth + 1)?;
                fs::unlinkat(dir, name, AtFlags::REMOVEDIR)?;
            } else {
                fs::unlinkat(dir, name, AtFlags::empty())?;
            }
        }
        Ok(())
    }
    clear(&root, &mut budget, 0)?;
    let parent = open_directory(identity.path.parent().context("invalid directory")?)?;
    let current = open_directory(&identity.path)?;
    ensure!(
        identity.matches(&current)?,
        "directory changed during deletion"
    );
    fs::unlinkat(
        &parent,
        identity
            .path
            .file_name()
            .context("missing directory name")?,
        AtFlags::REMOVEDIR,
    )?;
    Ok(())
}

fn list_entry(repo: &Repo, path: &Path, metadata: &File, machine: bool, bare: bool) -> Result<()> {
    let bytes = read_head(metadata)?;
    let value = bytes.trim_end_matches('\n');
    let branch = value.strip_prefix("ref: ");
    let id = match branch {
        Some(branch) => repo.read_ref(branch)?,
        None => Some(crate::object::Oid::parse(value.as_bytes())?),
    };
    if machine {
        println!("worktree {}", crate::compat::text(&path.to_string_lossy()));
        if bare {
            println!("bare\n");
        } else {
            println!("HEAD {}", id.unwrap_or(crate::object::Oid::ZERO));
            if let Some(branch) = branch {
                println!("branch {}\n", crate::compat::text(branch));
            } else {
                println!("detached\n");
            }
        }
    } else {
        println!(
            "{} {} {}",
            crate::compat::text(&path.to_string_lossy()),
            id.map(crate::output::short).unwrap_or_default(),
            if bare {
                "(bare)".into()
            } else if let Some(branch) = branch {
                format!(
                    "[{}]",
                    crate::compat::text(branch.trim_start_matches("refs/heads/"))
                )
            } else {
                "(detached HEAD)".into()
            }
        );
    }
    Ok(())
}

fn read_head(metadata: &File) -> Result<String> {
    let file = File::from(fs::openat(
        metadata,
        "HEAD",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    ensure!(file.metadata()?.is_file(), "HEAD must be a regular file");
    let mut value = String::new();
    file.take(2049).read_to_string(&mut value)?;
    ensure!(value.len() <= 2048, "HEAD exceeds limit");
    let text = value.trim_end_matches('\n');
    if let Some(branch) = text.strip_prefix("ref: ") {
        ensure!(branch.starts_with("refs/heads/"), "invalid linked HEAD");
        crate::repo::refname(branch)?;
    } else {
        crate::object::Oid::parse(text.as_bytes())?;
    }
    Ok(value)
}
