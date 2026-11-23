//! Everyday porcelain over inert objects and descriptor-relative worktree access.
use crate::{
    Cli,
    checkout::{self, Edit, Work},
    compat, ignore,
    index::{self, Entries, Entry},
    object::{MAX_OBJECTS, Oid, ReadObjects},
    porcelain,
    repo::{Pending, Repo, Store},
    revision,
};
use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{self, AtFlags, Mode, OFlags};
use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
};

pub fn run(
    cli: &Cli,
    repo: &Repo,
    store: &mut Store<'_>,
    name: &str,
    args: &[String],
) -> Result<()> {
    match name {
        "status" => status(repo, store, args),
        "add" => add(cli, repo, args),
        "restore" => restore(cli, repo, store, args),
        "switch" => switch(cli, repo, store, args),
        "stash" => stash(cli, repo, store, args),
        _ => bail!("unsupported workflow command"),
    }
}

pub(crate) fn paths(args: &[String], prefix: &std::path::Path) -> Result<Vec<Vec<u8>>> {
    args.iter()
        .map(|arg| {
            let mut value = arg.as_str();
            let literal = value.starts_with(":(literal)");
            if literal {
                value = &value[10..];
            }
            let top = value.starts_with(":(top)");
            if top {
                value = &value[6..];
            }
            if let Some(v) = value.strip_prefix(":(glob)") {
                value = v;
            }
            ensure!(
                !value.starts_with(':') && !value.starts_with('/'),
                "unsupported or absolute pathspec"
            );
            let mut parts: Vec<&str> = if top {
                Vec::new()
            } else {
                prefix
                    .to_str()
                    .context("non-UTF-8 working directory")?
                    .split('/')
                    .filter(|v| !v.is_empty() && *v != ".")
                    .collect()
            };
            for part in value.split('/') {
                match part {
                    "" | "." => {}
                    ".." => {
                        ensure!(parts.pop().is_some(), "pathspec escapes repository");
                    }
                    _ => parts.push(part),
                }
            }
            let value = parts.join("/").into_bytes();
            if !value.is_empty() {
                index::path(&value)?;
            }
            if literal {
                let mut v = b":(literal)".to_vec();
                v.extend(value);
                Ok(v)
            } else {
                Ok(value)
            }
        })
        .collect()
}
pub(crate) fn selected(path: &[u8], specs: &[Vec<u8>]) -> bool {
    specs.iter().any(|s| {
        s.is_empty()
            || path == s
            || pathspec(s, path)
            || path
                .strip_prefix(s.as_slice())
                .is_some_and(|v| v.starts_with(b"/"))
    })
}
fn pathspec(pattern: &[u8], value: &[u8]) -> bool {
    if let Some(p) = pattern.strip_prefix(b":(literal)") {
        return value == p || value.strip_prefix(p).is_some_and(|v| v.starts_with(b"/"));
    }
    if !pattern.iter().any(|b| b"*?[".contains(b)) {
        return false;
    }
    let mut row = vec![false; value.len() + 1];
    row[0] = true;
    let mut i = 0;
    while i < pattern.len() {
        let p = pattern[i];
        let mut end = i;
        if p == b'['
            && let Some(j) = pattern[i + 1..].iter().position(|b| *b == b']')
        {
            end = i + 1 + j;
        }
        let mut next = vec![false; row.len()];
        if p == b'*' {
            next[0] = row[0];
        }
        for (j, b) in value.iter().enumerate() {
            let matched = if end > i {
                let class = &pattern[i + 1..end];
                let neg = class.first().is_some_and(|b| *b == b'!' || *b == b'^');
                let class = if neg { &class[1..] } else { class };
                let mut found = false;
                let mut k = 0;
                while k < class.len() {
                    if k + 2 < class.len() && class[k + 1] == b'-' {
                        found |= class[k] <= *b && *b <= class[k + 2];
                        k += 3;
                    } else {
                        found |= class[k] == *b;
                        k += 1;
                    }
                }
                found != neg
            } else {
                p == b'?' || p == *b
            };
            next[j + 1] = if p == b'*' {
                row[j + 1] || next[j]
            } else {
                row[j] && matched
            };
        }
        row = next;
        i = end + 1;
    }
    row[value.len()]
}
fn intersects(path: &[u8], specs: &[Vec<u8>]) -> bool {
    selected(path, specs)
        || specs.iter().any(|s| s.iter().any(|b| b"*?[".contains(b)))
        || specs.iter().any(|s| {
            s.strip_prefix(b":(literal)")
                .unwrap_or(s)
                .strip_prefix(path)
                .is_some_and(|v| v.starts_with(b"/"))
        })
}
fn check_paths(specs: &[Vec<u8>], candidates: &BTreeSet<Vec<u8>>) -> Result<()> {
    for spec in specs {
        ensure!(
            spec.is_empty()
                || candidates
                    .iter()
                    .any(|p| selected(p, std::slice::from_ref(spec))),
            "pathspec {:?} did not match any files (ignored files require add -f)",
            String::from_utf8_lossy(spec)
        );
    }
    Ok(())
}
fn unchanged(repo: &Repo, original: &Option<Vec<u8>>, head: &[u8]) -> Result<()> {
    ensure!(
        repo.read("index", checkout::INDEX_LIMIT)? == *original
            && repo.read("HEAD", 2048)?.as_deref() == Some(head),
        "HEAD or index changed concurrently"
    );
    Ok(())
}
fn save_index(
    repo: &Repo,
    lock: &mut Pending,
    original: &Option<Vec<u8>>,
    head: &[u8],
    entries: &Entries,
) -> Result<()> {
    let encoded = index::encode(entries)?;
    unchanged(repo, original, head)?;
    lock.file.write_all(&encoded)?;
    lock.commit("index")
}

const DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
fn ignore_file(dir: &File) -> Result<Vec<u8>> {
    let stat = match fs::statat(dir, ".gitignore", AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    // Git does not follow a symlinked .gitignore either.
    if stat.st_mode & 0o170000 == 0o120000 {
        return Ok(Vec::new());
    }
    let file = File::from(fs::openat(
        dir,
        ".gitignore",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.nlink() == 1 && meta.dev() == dir.metadata()?.dev(),
        "unsafe ignore file"
    );
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1024 * 1024, "ignore file exceeds limit");
    Ok(bytes)
}
struct Scan<'a> {
    tracked: &'a Entries,
    specs: &'a [Vec<u8>],
    force: bool,
    device: u64,
    visited: usize,
    budget: usize,
    files: BTreeSet<Vec<u8>>,
    directories: BTreeSet<Vec<u8>>,
}
impl Scan<'_> {
    fn walk(
        &mut self,
        dir: &File,
        prefix: &[u8],
        rules: &mut Vec<ignore::Rule>,
        depth: usize,
    ) -> Result<()> {
        ensure!(depth <= 128, "worktree nesting exceeds limit");
        let original = rules.len();
        if !self.force {
            ignore::extend(rules, prefix, &ignore_file(dir)?)?;
        }
        for item in fs::Dir::read_from(dir)? {
            let item = item?;
            let name = item.file_name().to_bytes();
            if matches!(name, b"." | b".." | b".git") {
                continue;
            }
            self.visited += 1;
            ensure!(self.visited <= MAX_OBJECTS, "worktree entry limit exceeded");
            let mut path = prefix.to_vec();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(name);
            if !intersects(&path, self.specs) {
                continue;
            }
            index::path(&path)?;
            if self.tracked.get(&path).is_some_and(|e| e.mode == 0o160000) {
                continue;
            }
            let stat = fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
            ensure!(
                stat.st_dev == self.device,
                "cross-filesystem worktree traversal is unsupported"
            );
            let directory = stat.st_mode & 0o170000 == 0o40000;
            if !self.force && ignore::ignored(rules, &path, directory, &mut self.budget)? {
                continue;
            }
            if directory {
                let child = File::from(fs::openat(dir, name, DIR, Mode::empty())?);
                ensure!(
                    child.metadata()?.dev() == self.device,
                    "cross-filesystem worktree traversal is unsupported"
                );
                match fs::statat(&child, ".git", AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(_) => {
                        self.files.insert(path);
                    } // Never descend into another repository.
                    Err(rustix::io::Errno::NOENT) => {
                        self.directories.insert(path.clone());
                        self.walk(&child, &path, rules, depth + 1)?;
                    }
                    Err(e) => return Err(e.into()),
                }
            } else {
                self.files.insert(path);
            }
        }
        rules.truncate(original);
        Ok(())
    }
}
pub(crate) fn untracked(
    repo: &Repo,
    tracked: &Entries,
    specs: &[Vec<u8>],
    force: bool,
) -> Result<BTreeSet<Vec<u8>>> {
    Ok(scan_untracked(repo, tracked, specs, force)?.0)
}
type Paths = BTreeSet<Vec<u8>>;
pub(crate) fn scan_untracked(
    repo: &Repo,
    tracked: &Entries,
    specs: &[Vec<u8>],
    force: bool,
) -> Result<(Paths, Paths)> {
    let root = repo.worktree.as_ref().context("requires a working tree")?;
    let mut rules = Vec::new();
    if !force && let Some(bytes) = repo.read("info/exclude", 1024 * 1024)? {
        ignore::extend(&mut rules, b"", &bytes)?;
    }
    let mut scan = Scan {
        tracked,
        specs,
        force,
        device: root.metadata()?.dev(),
        visited: 0,
        budget: 64 * 1024 * 1024,
        files: BTreeSet::new(),
        directories: BTreeSet::new(),
    };
    scan.walk(root, b"", &mut rules, 0)?;
    scan.files
        .retain(|p| !tracked.contains_key(p) && selected(p, specs));
    scan.directories.retain(|p| {
        selected(p, specs)
            && !tracked
                .keys()
                .any(|t| t == p || t.starts_with(&[p.as_slice(), b"/"].concat()))
    });
    Ok((scan.files, scan.directories))
}

fn add(cli: &Cli, repo: &Repo, args: &[String]) -> Result<()> {
    let mut patch = false;
    let (mut all, mut update, mut force, mut end) = (false, false, false, false);
    let mut names = Vec::new();
    for arg in args {
        if end {
            names.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => end = true,
            "-p" | "--patch" => patch = true,
            "-A" | "--all" => all = true,
            "-u" | "--update" => update = true,
            "-f" | "--force" => force = true,
            a if a.starts_with('-') => bail!("unsupported add option {a}"),
            _ => names.push(arg.clone()),
        }
    }
    ensure!(!all || !update, "incompatible add options");
    ensure!(
        !names.is_empty() || all || update || patch,
        "add requires paths, --all, or --update"
    );
    let specs = if names.is_empty() {
        vec![Vec::new()]
    } else {
        paths(&names, &cli.prefix)?
    };
    let mut lock = Pending::new(repo.root.try_clone()?, "index.lock".into())?;
    let original = repo.read("index", checkout::INDEX_LIMIT)?;
    let head = repo.read("HEAD", 2048)?.context("missing HEAD")?;
    let (mut staged, mut conflicts) = porcelain::index_full(repo)?;
    ensure!(
        !patch || conflicts.is_empty(),
        "resolve conflicts with whole-file add before selecting hunks"
    );
    let original_staged = staged.clone();
    for (path, stages) in &conflicts {
        if let Some(entry) = stages.iter().flatten().next() {
            staged.insert(path.clone(), *entry);
        }
    }
    let mut targets: Entries = staged
        .iter()
        .filter(|(p, _)| selected(p, &specs))
        .map(|(p, e)| (p.clone(), *e))
        .collect();
    if !update && !patch {
        for path in untracked(repo, &staged, &specs, force)? {
            targets.insert(
                path,
                Entry {
                    mode: 0o100644,
                    oid: Oid::ZERO,
                },
            );
        }
    }
    check_paths(&specs, &targets.keys().cloned().collect())?;
    let mut actual = checkout::snapshot(repo, &targets)?;
    for (path, entry) in &mut actual {
        if entry.mode == 0o160000
            && let Some(id) = crate::submodule::head(repo, path)?
        {
            entry.oid = id;
        }
    }
    if patch {
        actual = crate::patch::select(repo, &mut Store::new(repo), &targets, &actual)?;
    }
    staged.retain(|p, _| !targets.contains_key(p));
    staged.extend(actual);
    conflicts.retain(|p, _| !selected(p, &specs));
    for path in conflicts.keys() {
        if !original_staged.contains_key(path) {
            staged.remove(path);
        }
    }
    let encoded = index::encode_full(&staged, &conflicts)?;
    unchanged(repo, &original, &head)?;
    lock.file.write_all(&encoded)?;
    lock.commit("index")
}

fn restore(cli: &Cli, repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    let mut patch = false;
    let (mut staged, mut worktree, mut end) = (false, false, false);
    let mut source = None;
    let mut names = Vec::new();
    let mut items = args.iter();
    while let Some(arg) = items.next() {
        if end {
            names.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => end = true,
            "-p" | "--patch" => patch = true,
            "--staged" | "-S" => staged = true,
            "--worktree" | "-W" => worktree = true,
            "--source" | "-s" => {
                source = Some(items.next().context("missing restore source")?.as_str())
            }
            a if a.starts_with("--source=") => source = Some(&a[9..]),
            a if a.starts_with('-') => bail!("unsupported restore option {a}"),
            _ => names.push(arg.clone()),
        }
    }
    ensure!(!names.is_empty() || patch, "restore requires paths");
    if names.is_empty() {
        names.push(".".into());
    }
    worktree |= !staged;
    let specs = paths(&names, &cli.prefix)?;
    let head_name = repo.head()?;
    let tip = repo.read_ref(&head_name)?;
    // Index-only restores need the same HEAD/ref protection as worktree edits.
    let _reference_lock = if !worktree {
        Some(repo.prepare_ref(&head_name, tip)?)
    } else {
        None
    };
    let _head_lock = if !worktree && head_name != "HEAD" {
        Some(Pending::new(repo.root.try_clone()?, "HEAD.lock".into())?)
    } else {
        None
    };
    let mut index_lock = if !worktree {
        Some(Pending::new(repo.root.try_clone()?, "index.lock".into())?)
    } else {
        None
    };
    ensure!(repo.head()? == head_name, "HEAD changed concurrently");
    let original = repo.read("index", checkout::INDEX_LIMIT)?;
    let head_bytes = repo.read("HEAD", 2048)?.context("missing HEAD")?;
    let original_index = porcelain::index(repo)?;
    let mut target = if let Some(source) = source {
        let id = revision::resolve(repo, store, source)?;
        porcelain::tree(store, id)?
    } else if staged {
        index::tree(store, tip)?
    } else {
        original_index.clone()
    };
    if patch {
        ensure!(
            !staged || !worktree,
            "patch restore accepts one destination at a time"
        );
        let current = if staged {
            original_index.clone()
        } else {
            checkout::snapshot(repo, &original_index)?
        };
        let selected_current = current
            .iter()
            .filter(|(p, _)| selected(p, &specs))
            .map(|(p, e)| (p.clone(), *e))
            .collect();
        let selected_target = target
            .iter()
            .filter(|(p, _)| selected(p, &specs))
            .map(|(p, e)| (p.clone(), *e))
            .collect();
        let chosen = crate::patch::select(repo, store, &selected_current, &selected_target)?;
        target.retain(|p, _| !selected(p, &specs));
        target.extend(chosen);
    }
    let candidates: BTreeSet<_> = original_index
        .keys()
        .chain(target.keys())
        .cloned()
        .collect();
    check_paths(&specs, &candidates)?;
    let mut after_index = original_index.clone();
    if staged {
        after_index.retain(|p, _| !selected(p, &specs));
        after_index.extend(
            target
                .iter()
                .filter(|(p, _)| selected(p, &specs))
                .map(|(p, e)| (p.clone(), *e)),
        );
    }
    if worktree {
        let before = checkout::snapshot(repo, &original_index)?;
        let mut after = before.clone();
        after.retain(|p, _| !selected(p, &specs));
        after.extend(target.into_iter().filter(|(p, _)| selected(p, &specs)));
        checkout::edit(
            repo,
            store,
            Edit {
                conflicts_before: Default::default(),
                conflicts_after: Default::default(),
                head: head_name,
                tip: tip.context("worktree restore requires an existing HEAD")?,
                before,
                after,
                index_before: original_index,
                index_after: after_index,
            },
        )
    } else {
        save_index(
            repo,
            index_lock.as_mut().unwrap(),
            &original,
            &head_bytes,
            &after_index,
        )
    }
}

fn switch(cli: &Cli, repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    let mut create = None;
    let mut tracking = false;
    let mut detached = false;
    let mut force_create = false;
    let mut value = None;
    let mut quiet = cli.quiet;
    let mut end = false;
    let mut items = args.iter();
    while let Some(arg) = items.next() {
        if !end {
            match arg.as_str() {
                "--" => {
                    end = true;
                    continue;
                }
                "-c" | "--create" | "-C" | "--force-create" => {
                    ensure!(create.is_none(), "duplicate create option");
                    create = Some(items.next().context("missing branch name")?.as_str());
                    force_create = matches!(arg.as_str(), "-C" | "--force-create");
                    continue;
                }
                "--track" | "-t" => {
                    tracking = true;
                    continue;
                }
                "-" => {}
                "-d" | "--detach" => {
                    detached = true;
                    continue;
                }
                "-q" | "--quiet" => {
                    quiet = true;
                    continue;
                }
                a if a.starts_with('-') => bail!("unsupported switch option {a}"),
                _ => {}
            }
        }
        ensure!(value.is_none(), "switch accepts one start point");
        value = Some(arg.as_str());
    }
    let previous = if value == Some("-") {
        Some(crate::history::previous(repo)?)
    } else {
        None
    };
    if let Some(previous) = &previous {
        value = Some(previous);
        detached |= previous.len() == 40 && previous.bytes().all(|b| b.is_ascii_hexdigit());
    }
    let mut inferred = None;
    let mut remote_start = None;
    if create.is_none()
        && !detached
        && let Some(name) = value
        && repo.read_ref(&format!("refs/heads/{name}"))?.is_none()
    {
        let candidates: Vec<_> = repo
            .refs()?
            .into_keys()
            .filter(|r| {
                r.starts_with("refs/remotes/")
                    && (r.ends_with(&format!("/{name}"))
                        || r.strip_prefix("refs/remotes/") == Some(name))
            })
            .collect();
        ensure!(
            candidates.len() == 1,
            "local branch missing and remote branch is missing or ambiguous"
        );
        remote_start = Some(candidates[0].clone());
        inferred = Some(if tracking {
            name.split_once('/')
                .map(|(_, v)| v)
                .unwrap_or(name)
                .to_owned()
        } else {
            name.to_owned()
        });
        tracking = true;
    }
    if let Some(name) = &inferred {
        create = Some(name);
        value = remote_start.as_deref();
    }
    let track_source = if tracking {
        Some(
            value
                .context("tracking needs a remote start point")?
                .to_owned(),
        )
    } else {
        None
    };
    if let Some(source) = &track_source {
        let short = source.strip_prefix("refs/remotes/").unwrap_or(source);
        let remote = short
            .split_once('/')
            .context("tracking requires REMOTE/BRANCH")?
            .0;
        let binding = crate::trust::named(repo, remote)?.context("remote is not registered")?;
        ensure!(
            short.starts_with(&format!("{}/", binding.remote)),
            "tracking requires a host-registered remote"
        );
    }
    ensure!(
        !detached || create.is_none(),
        "cannot create a detached branch"
    );
    if let Some(name) = create {
        let reference = format!("refs/heads/{}", crate::repo::branch(name)?);
        ensure!(
            force_create || repo.read_ref(&reference)?.is_none(),
            "branch already exists"
        );
        if value.is_none_or(|v| v == "HEAD") && repo.read_ref("HEAD")?.is_none() {
            ensure!(
                repo.read_ref(&reference)?.is_none(),
                "cannot reset an existing branch from unborn HEAD"
            );
            let current = repo.head()?;
            let _target_lock = repo.prepare_ref(&reference, None)?;
            let _current_lock = if current != reference {
                Some(repo.prepare_ref(&current, None)?)
            } else {
                None
            };
            let before = repo.read("HEAD", 2048)?.context("missing HEAD")?;
            let mut lock = Pending::new(repo.root.try_clone()?, "HEAD.lock".into())?;
            ensure!(
                repo.head()? == current
                    && repo.read("HEAD", 2048)?.as_deref() == Some(before.as_slice())
                    && repo.read_ref("HEAD")?.is_none(),
                "HEAD changed concurrently"
            );
            lock.file
                .write_all(format!("ref: {reference}\n").as_bytes())?;
            lock.commit("HEAD")?;
            if !quiet {
                eprintln!("Switched to a new branch '{name}'");
            }
            return Ok(());
        }
    } else if !detached {
        let name = value.context("switch requires a branch")?;
        let reference = format!("refs/heads/{}", crate::repo::branch(name)?);
        ensure!(
            repo.read_ref(&reference)?.is_some(),
            "local branch does not exist; use switch -c NAME START"
        );
        value = Some(name);
    }
    let value = value.unwrap_or("HEAD");
    if detached {
        let id = revision::commit(repo, store, value)?;
        porcelain::checkout(repo, store, &id.to_string(), None, false, quiet)
    } else {
        // Use an explicit local ref to avoid a same-named tag making switching ambiguous.
        let revision = if create.is_none() {
            format!("refs/heads/{value}")
        } else {
            value.into()
        };
        porcelain::checkout(
            repo,
            store,
            &revision,
            Some(create.unwrap_or(value)),
            false,
            quiet,
        )?;
        if let Some(source) = track_source {
            crate::history::track(repo, create.unwrap_or(value), &source)?;
        }
        Ok(())
    }
}

fn status(repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    let (mut short, mut nul, mut branch, mut v2) = (false, false, false, false);
    let mut untracked_mode = "normal";
    for arg in args {
        match arg.as_str() {
            "-s" | "--short" | "--porcelain" | "--porcelain=v1" => short = true,
            "--porcelain=v2" => {
                short = true;
                v2 = true;
            }
            "-z" => {
                nul = true;
                short = true;
            }
            "-b" | "--branch" => branch = true,
            "-sb" | "-bs" => {
                short = true;
                branch = true;
            }
            "-uno" | "--untracked-files=no" => untracked_mode = "no",
            "-uall" | "--untracked-files=all" | "-u" | "--untracked-files" => {
                untracked_mode = "all"
            }
            "-unormal" | "--untracked-files=normal" => untracked_mode = "normal",
            _ => bail!("unsupported status option {arg}"),
        }
    }
    let (staged, conflicts) = porcelain::index_full(repo)?;
    let before = index::tree(store, repo.read_ref("HEAD")?)?;
    let work = Work::new(repo)?;
    let mut rows = Vec::new();
    for path in before.keys().chain(staged.keys()).collect::<BTreeSet<_>>() {
        if conflicts.contains_key(path) {
            continue;
        }
        let a = before.get(path).copied();
        let b = staged.get(path).copied();
        let actual = if b.is_some_and(|e| e.mode == 0o160000) {
            b
        } else if b.is_some() {
            work.state(path)?
        } else {
            None
        };
        let (x, y) = (change(a, b), change(b, actual));
        if x != ' ' || y != ' ' {
            rows.push((x, y, path.clone(), None, 100u32, a, b, actual));
        }
    }
    // Pair staged additions and deletions. Exact matches first; bounded text similarity next.
    let deleted: Vec<_> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.0 == 'D')
        .map(|(i, _)| i)
        .collect();
    let added: Vec<_> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.0 == 'A')
        .map(|(i, _)| i)
        .collect();
    let mut removed = BTreeSet::new();
    let mut comparisons = 0;
    for add in added {
        let mut best = None;
        for del in &deleted {
            if removed.contains(del) {
                continue;
            }
            comparisons += 1;
            if comparisons > 10000 {
                break;
            }
            let a = rows[*del].5.unwrap();
            let b = rows[add].6.unwrap();
            if a.mode & 0o170000 != b.mode & 0o170000 {
                continue;
            }
            let score = if a.oid == b.oid {
                100
            } else if a.mode & 0o170000 == 0o100000 {
                let a = store.get(a.oid)?;
                let b = store.get(b.oid)?;
                if a.data.contains(&0)
                    || b.data.contains(&0)
                    || compat::diff_budget(&a.data, &b.data).is_err()
                {
                    continue;
                }
                let patch = diffy::create_patch_bytes(&a.data, &b.data);
                let deleted_bytes: usize = patch
                    .hunks()
                    .iter()
                    .flat_map(|h| h.lines())
                    .filter_map(|l| match l {
                        diffy::Line::Delete(v) => Some(v.len()),
                        _ => None,
                    })
                    .sum();
                ((a.data.len() - deleted_bytes) * 100 / a.data.len().max(b.data.len()).max(1))
                    as u32
            } else {
                0
            };
            if score >= 50 && best.is_none_or(|(_, s)| score > s) {
                best = Some((*del, score));
            }
        }
        if let Some((del, score)) = best {
            rows[add].0 = 'R';
            rows[add].3 = Some(rows[del].2.clone());
            rows[add].4 = score;
            rows[add].5 = rows[del].5;
            removed.insert(del);
        }
    }
    let mut rows: Vec<_> = rows
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !removed.contains(i))
        .map(|(_, r)| r)
        .collect();
    let mut tracked = staged.clone();
    for (p, stages) in &conflicts {
        if let Some(e) = stages.iter().flatten().next() {
            tracked.insert(p.clone(), *e);
        }
    }
    if untracked_mode != "no" {
        let mut names = BTreeSet::new();
        for mut path in untracked(repo, &tracked, &[Vec::new()], false)? {
            if untracked_mode == "normal" {
                for i in 0..path.len() {
                    if path[i] == b'/' && !tracked.keys().any(|p| p.starts_with(&path[..=i])) {
                        path.truncate(i + 1);
                        break;
                    }
                }
            }
            names.insert(path);
        }
        rows.extend(
            names
                .into_iter()
                .map(|p| ('?', '?', p, None, 100, None, None, None)),
        );
    }
    let head_name = repo.head()?;
    let head = head_name
        .strip_prefix("refs/heads/")
        .unwrap_or("HEAD (detached)");
    let tip = repo.read_ref("HEAD")?;
    let upstream = revision::upstream(repo)
        .ok()
        .and_then(|r| repo.read_ref(&r).ok().flatten().map(|id| (r, id)));
    let counts = if let Some((_, up)) = &upstream {
        tip.map(|tip| -> Result<_> {
            Ok((
                revision::history(store, tip, Some(*up), false)?.len(),
                revision::history(store, *up, Some(tip), false)?.len(),
            ))
        })
        .transpose()?
    } else {
        None
    };
    let mut out = std::io::stdout().lock();
    let end = if nul { "\0" } else { "\n" };
    if short && branch {
        if v2 {
            write!(
                out,
                "# branch.oid {}{end}# branch.head {}{end}",
                tip.map(|v| v.to_string()).unwrap_or("(initial)".into()),
                if head_name == "HEAD" {
                    "(detached)"
                } else {
                    head
                }
            )?;
            if let (Some((up, _)), Some((ahead, behind))) = (&upstream, counts) {
                write!(
                    out,
                    "# branch.upstream {}{end}# branch.ab +{ahead} -{behind}{end}",
                    compat::text(up.strip_prefix("refs/remotes/").unwrap_or(up))
                )?;
            }
        } else {
            write!(out, "## {}", compat::text(head))?;
            if let (Some((up, _)), Some((ahead, behind))) = (&upstream, counts) {
                write!(
                    out,
                    "...{}",
                    compat::text(up.strip_prefix("refs/remotes/").unwrap_or(up))
                )?;
                if ahead != 0 || behind != 0 {
                    let mut parts = Vec::new();
                    if ahead > 0 {
                        parts.push(format!("ahead {ahead}"));
                    }
                    if behind > 0 {
                        parts.push(format!("behind {behind}"));
                    }
                    write!(out, " [{}]", parts.join(", "))?;
                }
            }
            write!(out, "{end}")?;
        }
    }
    let mode = |e: Option<Entry>| e.map(|e| e.mode).unwrap_or(0);
    let oid = |e: Option<Entry>| e.map(|e| e.oid).unwrap_or(Oid::ZERO);
    if short {
        for (x, y, path, from, score, a, b, c) in &rows {
            if v2 {
                if *x == '?' {
                    write!(out, "? ")?;
                } else {
                    write!(
                        out,
                        "{} {}{} N... {:06o} {:06o} {:06o} {} {} ",
                        if from.is_some() { 2 } else { 1 },
                        if *x == ' ' { '.' } else { *x },
                        if *y == ' ' { '.' } else { *y },
                        mode(*a),
                        mode(*b),
                        mode(*c),
                        oid(*a),
                        oid(*b)
                    )?;
                    if from.is_some() {
                        write!(out, "R{score} ")?;
                    }
                }
            } else {
                write!(out, "{x}{y} ")?;
            }
            if nul {
                out.write_all(path)?;
                out.write_all(b"\0")?;
                if let Some(from) = from {
                    out.write_all(from)?;
                    out.write_all(b"\0")?;
                }
            } else if let Some(from) = from {
                if v2 {
                    writeln!(out, "{}\t{}", quote_path(path), quote_path(from))?;
                } else {
                    writeln!(out, "{} -> {}", quote_path(from), quote_path(path))?;
                }
            } else {
                writeln!(out, "{}", quote_path(path))?;
            }
        }
        for (path, stages) in &conflicts {
            let xy = match (
                stages[0].is_some(),
                stages[1].is_some(),
                stages[2].is_some(),
            ) {
                (false, true, true) => "AA",
                (true, true, false) => "UD",
                (true, false, true) => "DU",
                (true, false, false) => "DD",
                (false, true, false) => "AU",
                (false, false, true) => "UA",
                _ => "UU",
            };
            if v2 {
                write!(
                    out,
                    "u {xy} N... {:06o} {:06o} {:06o} {:06o} {} {} {} ",
                    mode(stages[0]),
                    mode(stages[1]),
                    mode(stages[2]),
                    mode(work.state(path)?),
                    oid(stages[0]),
                    oid(stages[1]),
                    oid(stages[2])
                )?;
            } else {
                write!(out, "{xy} ")?;
            }
            if nul {
                out.write_all(path)?;
                out.write_all(b"\0")?;
            } else {
                writeln!(out, "{}", quote_path(path))?;
            }
        }
    } else {
        writeln!(out, "On branch {}", compat::text(head))?;
        if tip.is_none() {
            writeln!(out, "\nNo commits yet")?;
        }
        if let Some((ahead, behind)) = counts
            && (ahead > 0 || behind > 0)
        {
            writeln!(
                out,
                "Your branch is {ahead} commit(s) ahead and {behind} commit(s) behind its upstream."
            )?;
        }
        for (title, column) in [
            ("Changes to be committed:", 0),
            ("Changes not staged for commit:", 1),
            ("Untracked files:", 2),
        ] {
            let group: Vec<_> = rows
                .iter()
                .filter(|r| match column {
                    0 => r.0 != ' ' && r.0 != '?',
                    1 => r.1 != ' ' && r.1 != '?',
                    _ => r.0 == '?',
                })
                .collect();
            if group.is_empty() {
                continue;
            }
            writeln!(out, "\n{title}")?;
            for (x, y, path, from, _, _, _, _) in group {
                let kind = match if column == 0 { x } else { y } {
                    'A' => "new file:   ",
                    'D' => "deleted:    ",
                    'T' => "typechange: ",
                    'R' => "renamed:    ",
                    '?' => "",
                    _ => "modified:   ",
                };
                if let Some(from) = from {
                    writeln!(out, "\t{kind}{} -> {}", quote_path(from), quote_path(path))?;
                } else {
                    writeln!(out, "\t{kind}{}", quote_path(path))?;
                }
            }
        }
        if !conflicts.is_empty() {
            writeln!(out, "\nUnmerged paths (resolve and add each file):")?;
            for path in conflicts.keys() {
                writeln!(out, "\t{}", quote_path(path))?;
            }
        }
        if rows.is_empty() && conflicts.is_empty() {
            writeln!(out, "nothing to commit, working tree clean")?;
        }
    }
    Ok(())
}
fn change(a: Option<Entry>, b: Option<Entry>) -> char {
    if a == b {
        ' '
    } else {
        match (a, b) {
            (None, Some(_)) => 'A',
            (Some(_), None) => 'D',
            (Some(a), Some(b)) if a.mode & 0o170000 != b.mode & 0o170000 => 'T',
            _ => 'M',
        }
    }
}
pub(crate) fn quote_path(path: &[u8]) -> String {
    if path
        .iter()
        .all(|b| (33..127).contains(b) && !b"\"\\".contains(b))
    {
        return String::from_utf8_lossy(path).into_owned();
    }
    let mut out = String::from("\"");
    for b in path {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            32..=126 => out.push(char::from(*b)),
            _ => out.push_str(&format!("\\{b:03o}")),
        }
    }
    out.push('"');
    out
}

fn stash(cli: &Cli, repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    crate::stash::run(cli, repo, store, args)
}
