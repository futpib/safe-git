//! Everyday porcelain over inert objects and descriptor-relative worktree access.
use crate::{
    Cli,
    checkout::{self, Edit, Work},
    compat, ignore,
    index::{self, Entries, Entry},
    object::{MAX_OBJECTS, Oid},
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
        "add" => add(repo, args),
        "restore" => restore(repo, store, args),
        "switch" => switch(cli, repo, store, args),
        "stash" => stash(cli, repo, store, args),
        _ => bail!("unsupported workflow command"),
    }
}

fn paths(args: &[String]) -> Result<Vec<Vec<u8>>> {
    args.iter()
        .map(|arg| {
            let mut value = arg.as_str();
            while let Some(rest) = value.strip_prefix("./") {
                value = rest;
            }
            if value == "." || value.is_empty() && arg.starts_with("./") {
                return Ok(Vec::new());
            }
            let value = value.trim_end_matches('/').as_bytes();
            index::path(value)?;
            Ok(value.to_vec())
        })
        .collect()
}
fn selected(path: &[u8], specs: &[Vec<u8>]) -> bool {
    specs.iter().any(|s| {
        s.is_empty()
            || path == s
            || path
                .strip_prefix(s.as_slice())
                .is_some_and(|v| v.starts_with(b"/"))
    })
}
fn intersects(path: &[u8], specs: &[Vec<u8>]) -> bool {
    selected(path, specs)
        || specs
            .iter()
            .any(|s| s.strip_prefix(path).is_some_and(|v| v.starts_with(b"/")))
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
                    Err(rustix::io::Errno::NOENT) => self.walk(&child, &path, rules, depth + 1)?,
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
fn untracked(
    repo: &Repo,
    tracked: &Entries,
    specs: &[Vec<u8>],
    force: bool,
) -> Result<BTreeSet<Vec<u8>>> {
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
    };
    scan.walk(root, b"", &mut rules, 0)?;
    scan.files.retain(|p| !tracked.contains_key(p));
    Ok(scan.files)
}

fn add(repo: &Repo, args: &[String]) -> Result<()> {
    let (mut all, mut update, mut force, mut end) = (false, false, false, false);
    let mut names = Vec::new();
    for arg in args {
        if end {
            names.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => end = true,
            "-A" | "--all" => all = true,
            "-u" | "--update" => update = true,
            "-f" | "--force" => force = true,
            a if a.starts_with('-') => bail!("unsupported add option {a}"),
            _ => names.push(arg.clone()),
        }
    }
    ensure!(!all || !update, "incompatible add options");
    ensure!(
        !names.is_empty() || all || update,
        "add requires paths, --all, or --update"
    );
    let specs = if names.is_empty() {
        vec![Vec::new()]
    } else {
        paths(&names)?
    };
    let mut lock = Pending::new(repo.root.try_clone()?, "index.lock".into())?;
    let original = repo.read("index", checkout::INDEX_LIMIT)?;
    let head = repo.read("HEAD", 2048)?.context("missing HEAD")?;
    let mut staged = porcelain::index(repo)?;
    let mut targets: Entries = staged
        .iter()
        .filter(|(p, _)| selected(p, &specs))
        .map(|(p, e)| (p.clone(), *e))
        .collect();
    if !update {
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
    let actual = checkout::snapshot(repo, &targets)?;
    staged.retain(|p, _| !targets.contains_key(p));
    staged.extend(actual);
    save_index(repo, &mut lock, &original, &head, &staged)
}

fn restore(repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
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
    ensure!(!names.is_empty(), "restore requires paths");
    worktree |= !staged;
    let specs = paths(&names)?;
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
    let target = if let Some(source) = source {
        let id = revision::resolve(repo, store, source)?;
        porcelain::tree(store, id)?
    } else if staged {
        index::tree(store, tip)?
    } else {
        original_index.clone()
    };
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
        )
    }
}

fn status(repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    let (mut short, mut nul, mut branch) = (false, false, false);
    let mut untracked_mode = "normal";
    for arg in args {
        match arg.as_str() {
            "-s" | "--short" | "--porcelain" | "--porcelain=v1" => short = true,
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
    let staged = porcelain::index(repo)?;
    let before = index::tree(store, repo.read_ref("HEAD")?)?;
    let work = Work::new(repo)?;
    let mut rows = Vec::new();
    for path in before.keys().chain(staged.keys()).collect::<BTreeSet<_>>() {
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
            rows.push((x, y, path.clone()));
        }
    }
    if untracked_mode != "no" {
        let mut names = BTreeSet::new();
        for mut path in untracked(repo, &staged, &[Vec::new()], false)? {
            if untracked_mode == "normal" {
                for i in 0..path.len() {
                    if path[i] == b'/' && !staged.keys().any(|p| p.starts_with(&path[..=i])) {
                        path.truncate(i + 1);
                        break;
                    }
                }
            }
            names.insert(path);
        }
        rows.extend(names.into_iter().map(|p| ('?', '?', p)));
    }
    let head = repo.head()?;
    let head = head
        .strip_prefix("refs/heads/")
        .unwrap_or("HEAD (detached)");
    let mut out = std::io::stdout().lock();
    if short {
        if branch {
            write!(
                out,
                "## {}{}",
                compat::text(head),
                if nul { "\0" } else { "\n" }
            )?;
        }
        for (x, y, path) in rows {
            write!(out, "{x}{y} ")?;
            if nul {
                out.write_all(&path)?;
                out.write_all(b"\0")?;
            } else {
                writeln!(out, "{}", quote_path(&path))?;
            }
        }
    } else {
        writeln!(out, "On branch {}", compat::text(head))?;
        if repo.read_ref("HEAD")?.is_none() {
            writeln!(out, "\nNo commits yet")?;
        }
        for (title, column) in [
            ("Changes to be committed:", 0),
            ("Changes not staged for commit:", 1),
            ("Untracked files:", 2),
        ] {
            let group: Vec<_> = rows
                .iter()
                .filter(|(x, y, _)| match column {
                    0 => *x != ' ' && *x != '?',
                    1 => *y != ' ' && *y != '?',
                    _ => *x == '?',
                })
                .collect();
            if group.is_empty() {
                continue;
            }
            writeln!(out, "\n{title}")?;
            for (x, y, path) in group {
                let kind = match if column == 0 { x } else { y } {
                    'A' => "new file:   ",
                    'D' => "deleted:    ",
                    'T' => "typechange: ",
                    '?' => "",
                    _ => "modified:   ",
                };
                writeln!(out, "\t{kind}{}", quote_path(path))?;
            }
        }
        if rows.is_empty() {
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
fn quote_path(path: &[u8]) -> String {
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
