//! Single-branch clone into private staging, then atomic directory publication.
use crate::{
    checkout, network,
    object::ReadObjects,
    repo::{Repo, Store},
    transport::Auth,
    trust,
};
use anyhow::{Context, Result, ensure};
use rustix::fs::{self, CWD, RenameFlags};
use std::{
    fs::{File, Metadata},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

struct Destination {
    path: PathBuf,
    existing: Option<Metadata>,
    staging: tempfile::TempDir,
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
        let parent = parent
            .canonicalize()
            .context("open clone destination parent")?;
        let path = parent.join(name);
        let existing = match std::fs::symlink_metadata(&path) {
            Ok(meta) => {
                ensure!(
                    meta.is_dir(),
                    "clone destination must be a real directory, not a file or symlink"
                );
                ensure!(
                    std::fs::read_dir(&path)?.next().is_none(),
                    "clone destination is not empty"
                );
                Some(meta)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let staging = tempfile::Builder::new()
            .prefix(".safe-git-clone-")
            .tempdir_in(parent)?;
        Ok(Self {
            path,
            existing,
            staging,
        })
    }
    fn publish(self) -> Result<()> {
        if let Some(original) = self.existing {
            let current = std::fs::symlink_metadata(&self.path)?;
            ensure!(
                current.is_dir()
                    && current.dev() == original.dev()
                    && current.ino() == original.ino(),
                "clone destination changed"
            );
            // rename can replace an empty directory, but never a populated
            // directory, regular file, or symlink with our directory.
            fs::renameat(CWD, self.staging.path(), CWD, &self.path)?;
        } else {
            fs::renameat_with(
                CWD,
                self.staging.path(),
                CWD,
                &self.path,
                RenameFlags::NOREPLACE,
            )
            .context("publish clone without replacing an existing destination")?;
        }
        // The staging path was moved. Disarm its destructor before returning.
        let _ = self.staging.keep();
        File::open(self.path.parent().context("clone parent")?)?.sync_all()?;
        Ok(())
    }
}

pub struct Options<'a> {
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
    let objects = network::fetch(&url, auth, &selected, &[]).await?;
    let mut store = Store::new(&repo);
    store.add(objects)?;
    for (name, id) in &selected {
        if name.starts_with("refs/heads/") {
            store.get(*id)?.commit()?;
        }
    }
    let ids = crate::object::reachable_many_since(
        &mut store,
        &selected.values().copied().collect::<Vec<_>>(),
        &std::collections::HashSet::new(),
    )?;
    for id in ids {
        repo.write_object(store.get(id)?.as_ref())?;
    }
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
            checkout::merge(&repo, &mut store, &repo.head()?, None, tip)?;
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
    destination.publish()?;
    // Bind both directory identity and its final location in trusted host state.
    trust::save(
        &repo,
        &trust::Binding {
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
