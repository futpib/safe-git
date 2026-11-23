//! Single-branch clone into private staging, then atomic directory publication.
use crate::{
    checkout, network,
    object::ReadObjects,
    repo::{Repo, Store},
    transport::Auth,
    trust,
};
use anyhow::{Context, Result, ensure};
use rustix::fs::{self, CWD, Mode, OFlags, RenameFlags};
use std::{ffi::OsString, fs::File, os::fd::AsRawFd, path::Path};

struct Destination {
    // TempDir uses /proc/self/fd/PARENT; clean it before closing that descriptor.
    staging: tempfile::TempDir,
    parent: File,
    name: OsString,
    existing: Option<(u64, u64)>,
}

impl Destination {
    fn new(path: &Path) -> Result<Self> {
        let name = path
            .file_name()
            .context("clone requires a destination directory name")?;
        // The caller selects the parent directory, including zond's /proc/fd
        // staging parent. Repository content never supplies this path.
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = File::from(fs::open(
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        let existing = match fs::statat(&parent, name, fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => {
                ensure!(
                    stat.st_mode & 0o170000 == 0o40000,
                    "clone destination must be a real directory"
                );
                let child = File::from(fs::openat(
                    &parent,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?);
                for entry in fs::Dir::read_from(&child)? {
                    ensure!(
                        matches!(entry?.file_name().to_bytes(), b"." | b".."),
                        "clone destination is not empty"
                    );
                }
                Some((stat.st_dev, stat.st_ino))
            }
            Err(rustix::io::Errno::NOENT) => None,
            Err(e) => return Err(e.into()),
        };
        let staging = tempfile::Builder::new()
            .prefix(".safe-git-clone-")
            .tempdir_in(format!("/proc/self/fd/{}", parent.as_raw_fd()))?;
        Ok(Self {
            parent,
            name: name.into(),
            existing,
            staging,
        })
    }

    fn publish(self) -> Result<()> {
        if let Some(original) = self.existing {
            let current = fs::statat(&self.parent, &self.name, fs::AtFlags::SYMLINK_NOFOLLOW)?;
            ensure!(
                current.st_mode & 0o170000 == 0o40000
                    && (current.st_dev, current.st_ino) == original,
                "clone destination changed"
            );
            // rename can replace an empty directory, but never a populated
            // directory, regular file, or symlink with our directory.
            fs::renameat(CWD, self.staging.path(), &self.parent, &self.name)?;
        } else {
            fs::renameat_with(
                CWD,
                self.staging.path(),
                &self.parent,
                &self.name,
                RenameFlags::NOREPLACE,
            )
            .context("publish clone without replacing an existing destination")?;
        }
        // The staging path was moved. Disarm its destructor before returning.
        let _ = self.staging.keep();
        self.parent.sync_all()?;
        Ok(())
    }
}

pub struct Options<'a> {
    pub filter: Option<&'a str>,
    pub no_checkout: bool,
    pub depth: Option<u32>,
    pub branch: Option<&'a str>,
    pub remote: &'a str,
    pub single: bool,
    pub bare: bool,
    pub mirror: bool,
    pub quiet: bool,
}
pub async fn run(
    destination: &Path,
    display: &Path,
    url: &str,
    auth: &Auth,
    options: Options<'_>,
) -> Result<()> {
    if let Some(filter) = options.filter {
        crate::partial::filter(filter)?;
    }
    let url = network::normalize(url)?;
    crate::repo::tracking(options.remote, "check")?;
    let destination = Destination::new(destination)?;
    if !options.quiet {
        eprintln!(
            "Cloning into {}'{}'...",
            if options.bare { "bare repository " } else { "" },
            display.display().to_string().escape_debug()
        );
    }
    let ad = network::discover(&url, auth).await?;
    let (branch, expected) = ad.clone_branch(options.branch)?;
    ensure!(
        branch != "HEAD" && !branch.starts_with("HEAD/"),
        "branch conflicts with remote HEAD"
    );
    let repo = if options.bare {
        Repo::init_bare_existing(destination.staging.path(), &branch)?
    } else {
        Repo::init_worktree(destination.staging.path(), &branch)?
    };
    let selected: std::collections::HashMap<_, _> = ad
        .refs
        .iter()
        .filter(|(name, _)| {
            !name.ends_with("^{}")
                && if options.mirror {
                    name.starts_with("refs/")
                } else if options.single {
                    **name == format!("refs/heads/{branch}")
                } else {
                    name.starts_with("refs/heads/") || name.starts_with("refs/tags/")
                }
        })
        .map(|(n, id)| (n.clone(), *id))
        .collect();
    ensure!(
        !selected.keys().any(|n| n.starts_with("refs/replace/")),
        "replacement refs are unsupported"
    );
    let fetched = network::fetch_options(
        &url,
        auth,
        &selected,
        &[],
        &network::FetchOptions {
            depth: options.depth,
            filter: options.filter.map(str::to_owned),
            ..Default::default()
        },
    )
    .await?;
    if options.filter.is_some() {
        crate::partial::store_pack(&repo, &fetched.objects)?;
    }
    let mut store = Store::new(&repo);
    store.shallow = fetched.shallow;
    store.add(fetched.objects)?;
    for (name, id) in &selected {
        if name.starts_with("refs/heads/") {
            store.get(*id)?.commit()?;
        }
    }
    let ids = crate::partial::persist(
        &repo,
        &mut store,
        &selected.values().copied().collect::<Vec<_>>(),
        options.filter.is_some(),
        &std::collections::HashSet::new(),
    )?;
    ensure!(
        store.shallow.iter().all(|id| ids.contains(id)),
        "unreachable shallow boundary"
    );
    for id in store.shallow.clone() {
        store.get(id)?.commit()?;
    }
    crate::shallow::save(&repo, &store.shallow)?;
    for (name, id) in &selected {
        let dest = if options.bare || name.starts_with("refs/tags/") {
            name.clone()
        } else {
            crate::repo::tracking(options.remote, name.strip_prefix("refs/heads/").unwrap())?
        };
        repo.update_ref(&dest, None, *id)?;
    }
    if let Some(tip) = expected {
        if !options.bare {
            if !options.no_checkout {
                if options.filter.is_some() {
                    let entries = crate::index::tree(&mut store, Some(tip))?;
                    let missing: std::collections::HashMap<_, _> = entries
                        .values()
                        .filter(|e| e.mode != 0o160000 && store.get(e.oid).is_err())
                        .map(|e| (e.oid.to_string(), e.oid))
                        .collect();
                    if !missing.is_empty() {
                        let objects = network::fetch_options(
                            &url,
                            auth,
                            &missing,
                            &[],
                            &network::FetchOptions {
                                raw: true,
                                ..Default::default()
                            },
                        )
                        .await?
                        .objects;
                        for obj in objects.values() {
                            repo.write_object(obj)?;
                        }
                        store.add(objects)?;
                    }
                }
                checkout::merge(&repo, &mut store, &repo.head()?, None, tip)?;
            } else {
                repo.update_ref(&repo.head()?, None, tip)?;
            }
            repo.atomic(
                &format!("refs/remotes/{}/HEAD", options.remote),
                format!("ref: refs/remotes/{}/{branch}\n", options.remote).as_bytes(),
            )?;
        }
    } else if !options.quiet {
        eprintln!("warning: You appear to have cloned an empty repository.");
    }
    let quote = crate::config::quote;
    let refspec = if options.mirror {
        "+refs/*:refs/*".to_owned()
    } else if options.single {
        format!(
            "+refs/heads/{branch}:refs/remotes/{}/{branch}",
            options.remote
        )
    } else {
        format!("+refs/heads/*:refs/remotes/{}/*", options.remote)
    };
    repo.atomic("config", format!(
        "[core]\n\trepositoryformatversion = 0\n\tbare = {}\n[remote {}]\n\turl = {}\n\tfetch = {}\n[branch {}]\n\tremote = {}\n\tmerge = {}\n",
        options.bare,quote(options.remote),quote(&url),quote(&refspec),quote(&branch),quote(options.remote),quote(&format!("refs/heads/{branch}")),
    ).as_bytes())?;
    if let Some(filter) = options.filter {
        let mut config = repo.read("config", 8 * 1024 * 1024)?.unwrap();
        config.extend_from_slice(
            format!(
                "\n[remote {}]\n\tpromisor = true\n\tpartialCloneFilter = {}\n",
                quote(options.remote),
                quote(filter)
            )
            .as_bytes(),
        );
        repo.atomic("config", &config)?;
    }
    destination.publish()?;
    // Bind both directory identity and its final location in trusted host state.
    trust::save(
        &repo,
        &trust::Binding {
            filter: options.filter.map(str::to_owned),
            url,
            remote: options.remote.into(),
            branch,
            single: options.single,
            mirror: options.mirror,
            bare: options.bare,
            upstreams: Default::default(),
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publication_uses_the_opened_parent_after_path_replacement() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("parent");
        let moved = root.path().join("moved");
        let outside = root.path().join("outside");
        std::fs::create_dir(&parent).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let stage = Destination::new(&parent.join("repo")).unwrap();
        std::fs::rename(&parent, &moved).unwrap();
        std::os::unix::fs::symlink(&outside, &parent).unwrap();
        stage.publish().unwrap();
        assert!(moved.join("repo").is_dir());
        assert!(!outside.join("repo").exists());
    }
    #[test]
    fn publication_never_clobbers_files_or_populated_directories() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("clone");
        let stage = Destination::new(&path).unwrap();
        std::fs::write(&path, "preserve").unwrap();
        assert!(stage.publish().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "preserve");
        assert!(Destination::new(&path).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let stage = Destination::new(&path).unwrap();
        std::fs::write(path.join("keep"), "preserve").unwrap();
        assert!(stage.publish().is_err());
        assert_eq!(
            std::fs::read_to_string(path.join("keep")).unwrap(),
            "preserve"
        );
        assert!(Destination::new(&path).is_err());
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(Destination::new(&link).is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
    }
}
