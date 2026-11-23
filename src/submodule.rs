//! Gitlinks and .gitmodules are inert data. Only caller-registered URLs are fetched.
use crate::{
    Cli,
    checkout::{self, Edit, Work},
    config,
    index::{self, Entry},
    linked, network,
    object::{Kind, Object, Oid, ReadObjects},
    porcelain,
    repo::{Repo, Store},
    trust, workflow,
};
use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{self, Mode, OFlags};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::PathBuf,
};
const DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
fn record(repo: &Repo) -> Result<PathBuf> {
    let identity = linked::Identity::of(&repo.root)?;
    let hash = crate::object::hash(&[&serde_json::to_vec(&identity)?])?;
    Ok(trust::directory()?.join(format!("submodules-{hash}.json")))
}
fn registrations(repo: &Repo) -> Result<BTreeMap<String, String>> {
    let path = record(repo)?;
    let file = match fs::open(
        &path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(f) => File::from(f),
        Err(rustix::io::Errno::NOENT) => return Ok(BTreeMap::new()),
        Err(e) => return Err(e.into()),
    };
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.nlink() == 1 && meta.mode() & 0o022 == 0,
        "unsafe submodule registration"
    );
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "submodule registration exceeds limit"
    );
    let registrations: BTreeMap<String, String> = serde_json::from_slice(&bytes)?;
    for path in registrations.keys() {
        index::path(path.as_bytes())?;
    }
    Ok(registrations)
}
fn save(repo: &Repo, values: &BTreeMap<String, String>) -> Result<()> {
    let path = record(repo)?;
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    let bytes = serde_json::to_vec(values)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "submodule registration exceeds limit"
    );
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}
fn parent(repo: &Repo, path: &[u8], create: bool) -> Result<(File, Vec<u8>)> {
    index::path(path)?;
    let mut parts: Vec<_> = path.split(|b| *b == b'/').collect();
    let name = parts.pop().unwrap().to_vec();
    let mut dir = repo
        .worktree
        .as_ref()
        .context("requires a working tree")?
        .try_clone()?;
    let device = dir.metadata()?.dev();
    for part in parts {
        if create {
            match fs::mkdirat(&dir, part, Mode::from_raw_mode(0o755)) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(e) => return Err(e.into()),
            }
        }
        dir = File::from(fs::openat(&dir, part, DIR, Mode::empty())?);
        ensure!(
            dir.metadata()?.dev() == device,
            "submodule crosses a filesystem boundary"
        );
    }
    Ok((dir, name))
}
fn child(repo: &Repo, path: &[u8]) -> Result<Repo> {
    let (dir, name) = parent(repo, path, false)?;
    let path = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
        .join(std::ffi::OsStr::from_bytes(&name));
    Repo::open(&path)
}
use std::os::unix::ffi::OsStrExt;
pub fn head(repo: &Repo, path: &[u8]) -> Result<Option<Oid>> {
    let (dir, name) = parent(repo, path, false)?;
    let child = File::from(fs::openat(&dir, name.as_slice(), DIR, Mode::empty())?);
    match fs::statat(&child, ".git", fs::AtFlags::SYMLINK_NOFOLLOW) {
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(e) => Err(e.into()),
        Ok(_) => self::child(repo, path)?.read_ref("HEAD"),
    }
}
pub async fn run(cli: &Cli, repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    let command = args.first().map(String::as_str).unwrap_or("status");
    let mut url = None;
    let mut force = false;
    let mut names = Vec::new();
    let mut end = false;
    let mut items = args.iter().skip(1);
    while let Some(arg) = items.next() {
        if end {
            names.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => end = true,
            "--url" => url = Some(items.next().context("missing submodule URL")?.clone()),
            "--init" | "--checkout" => {}
            "-f" | "--force" => force = true,
            a if a.starts_with('-') => bail!(
                "unsupported submodule option {a}; recursive endpoints must be registered individually"
            ),
            _ => names.push(arg.clone()),
        }
    }
    let staged = porcelain::index(repo)?;
    let mut registered = registrations(repo)?;
    if command == "add" {
        ensure!(
            names.len() == 2 && url.is_none(),
            "submodule add requires URL PATH"
        );
        let endpoint = network::normalize_from(&names[0], &cli.repository)?;
        let spec = workflow::paths(&[names[1].clone()], &cli.prefix)?.remove(0);
        index::path(&spec)?;
        ensure!(
            !spec.iter().any(|b| b"*?[".contains(b)),
            "submodule path must be literal"
        );
        ensure!(!staged.contains_key(&spec), "submodule path is tracked");
        let name = std::str::from_utf8(&spec)?;
        let id = clone_at(cli, repo, &spec, &endpoint).await?;
        registered.insert(name.into(), endpoint.clone());
        save(repo, &registered)?;
        let mut after_index = staged.clone();
        after_index.insert(
            spec.clone(),
            Entry {
                mode: 0o160000,
                oid: id,
            },
        );
        let work = Work::new(repo)?;
        let mut before = checkout::snapshot(repo, &staged)?;
        let modules = b".gitmodules".to_vec();
        if let Some(entry) = work.state(&modules)? {
            ensure!(
                entry.mode == 0o100644 || entry.mode == 0o100755,
                ".gitmodules must be a regular file"
            );
            before.extend(checkout::snapshot(
                repo,
                &[(modules.clone(), entry)].into(),
            )?);
        }
        let mut data = match before.get(&modules) {
            Some(e) => store.get(e.oid)?.data.clone(),
            None => Vec::new(),
        };
        config::parse(&data)?;
        data.extend_from_slice(
            format!(
                "\n[submodule {}]\n\tpath = {}\n\turl = {}\n",
                config::quote(name),
                config::quote(name),
                config::quote(&endpoint)
            )
            .as_bytes(),
        );
        let obj = Object {
            kind: Kind::Blob,
            data,
        };
        repo.write_object(&obj)?;
        let entry = Entry {
            mode: 0o100644,
            oid: obj.oid()?,
        };
        after_index.insert(modules.clone(), entry);
        let mut after = before.clone();
        after.insert(modules, entry);
        after.insert(
            spec,
            Entry {
                mode: 0o160000,
                oid: id,
            },
        );
        checkout::edit(
            repo,
            store,
            Edit {
                head: repo.head()?,
                tip: repo.read_ref("HEAD")?.unwrap_or(Oid::ZERO),
                before,
                after,
                index_before: staged,
                index_after: after_index,
                conflicts_before: Default::default(),
                conflicts_after: Default::default(),
            },
        )?;
        return Ok(());
    }
    let specs = if names.is_empty() {
        vec![Vec::new()]
    } else {
        workflow::paths(&names, &cli.prefix)?
    };
    let selected: Vec<_> = staged
        .iter()
        .filter(|(p, e)| e.mode == 0o160000 && workflow::selected(p, &specs))
        .collect();
    ensure!(
        names.is_empty() || !selected.is_empty(),
        "no matching submodule"
    );
    if let Some(url) = url {
        ensure!(
            matches!(command, "init" | "update") && selected.len() == 1,
            "--url requires one submodule path"
        );
        registered.insert(
            std::str::from_utf8(selected[0].0)?.into(),
            network::normalize_from(&url, &cli.repository)?,
        );
        save(repo, &registered)?;
    }
    for (path, entry) in selected {
        let name = std::str::from_utf8(path)?;
        match command {
            "status" => {
                let actual = match child(repo, path) {
                    Ok(r) => r.read_ref("HEAD")?,
                    Err(_) => None,
                };
                println!(
                    "{}{} {}",
                    match actual {
                        None => '-',
                        Some(id) if id != entry.oid => '+',
                        _ => ' ',
                    },
                    actual.unwrap_or(entry.oid),
                    workflow::quote_path(path)
                );
            }
            "init" => {
                ensure!(
                    registered.contains_key(name),
                    "submodule endpoint is untrusted; register it with submodule init --url URL -- PATH"
                );
            }
            "update" => {
                let endpoint = registered.get(name).context(
                    "submodule endpoint is untrusted; use submodule init --url URL -- PATH",
                )?;
                let (dir, leaf) = parent(repo, path, true)?;
                let destination = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
                    .join(std::ffi::OsStr::from_bytes(&leaf));
                let populated = match fs::openat(&dir, leaf.as_slice(), DIR, Mode::empty()) {
                    Ok(fd) => {
                        let d = File::from(fd);
                        fs::statat(&d, ".git", fs::AtFlags::SYMLINK_NOFOLLOW).is_ok()
                    }
                    Err(rustix::io::Errno::NOENT) => false,
                    Err(e) => return Err(e.into()),
                };
                if !populated {
                    clone_at(cli, repo, path, endpoint).await?;
                }
                let sub = Repo::open(&destination)?;
                let mut objects = Store::new(&sub);
                let ad = network::discover(endpoint, &cli.auth()?).await?;
                let refs = ad
                    .refs
                    .into_iter()
                    .filter(|(r, _)| r.starts_with("refs/") && !r.ends_with("^{}"))
                    .collect();
                objects.add(network::fetch(endpoint, &cli.auth()?, &refs, &[]).await?)?;
                for id in crate::object::reachable(&mut objects, entry.oid)? {
                    sub.write_object(objects.get(id)?.as_ref())?;
                }
                porcelain::checkout(
                    &sub,
                    &mut objects,
                    &entry.oid.to_string(),
                    None,
                    force,
                    cli.quiet,
                )?;
            }
            "deinit" => {
                let sub = child(repo, path)?;
                let mut objects = Store::new(&sub);
                let index = porcelain::index(&sub)?;
                if !force {
                    ensure!(
                        index == index::tree(&mut objects, sub.read_ref("HEAD")?)?
                            && checkout::snapshot(&sub, &index)? == index,
                        "submodule has local changes"
                    );
                    ensure!(
                        workflow::untracked(&sub, &index, &[Vec::new()], true)?.is_empty(),
                        "submodule contains untracked files"
                    );
                }
                let identity =
                    linked::Identity::of(sub.worktree.as_ref().context("missing worktree")?)?;
                // Preserve the entire local repository, including unpublished commits.
                let (backup_dir, _) = repo.parent("safe-git-submodules/.check", true)?;
                let key = crate::object::hash(&[path])?.to_string();
                let (parent, leaf) = parent(repo, path, false)?;
                ensure!(
                    identity.matches(sub.worktree.as_ref().unwrap())?,
                    "submodule changed concurrently"
                );
                fs::renameat_with(
                    &parent,
                    leaf.as_slice(),
                    &backup_dir,
                    key.as_str(),
                    fs::RenameFlags::NOREPLACE,
                )?;
                backup_dir.sync_all()?;
                if !cli.quiet {
                    println!("Saved submodule data in .git/safe-git-submodules/{key}");
                }

                registered.remove(name);
                save(repo, &registered)?;
            }
            _ => bail!(
                "unsupported submodule command {command}; repository-selected commands are never run"
            ),
        }
    }
    Ok(())
}
async fn clone_at(cli: &Cli, repo: &Repo, path: &[u8], url: &str) -> Result<Oid> {
    let (dir, name) = parent(repo, path, true)?;
    let destination = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
        .join(std::ffi::OsStr::from_bytes(&name));
    crate::clone::run(
        &destination,
        &destination,
        url,
        &cli.auth()?,
        crate::clone::Options {
            depth: None,
            branch: None,
            remote: "origin",
            single: false,
            bare: false,
            mirror: false,
            quiet: cli.quiet,
        },
    )
    .await?;
    let sub = Repo::open(&destination)?;
    sub.read_ref("HEAD")?.context("empty submodule")
}
