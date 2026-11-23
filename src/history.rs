//! Inert branch and history operations. Reflogs are bounded data, never commands.
use crate::{
    Cli, compat, index,
    object::{Oid, ancestor},
    porcelain,
    repo::{Pending, Repo, Store},
    revision, trust,
};
use anyhow::{Context, Result, bail, ensure};
use std::io::Write;

const LIMIT: usize = 8 * 1024 * 1024;
pub fn log_ids(line: &[u8]) -> Result<(Oid, Oid)> {
    ensure!(
        line.ends_with(b"\n") && line.contains(&b'\t'),
        "invalid reflog"
    );
    let mut parts = line.splitn(3, |b| *b == b' ');
    let old = Oid::parse(parts.next().unwrap_or_default())?;
    let new = Oid::parse(parts.next().unwrap_or_default())?;
    ensure!(
        old.format() == new.format() && parts.next().is_some_and(|v| !v.is_empty()),
        "invalid reflog"
    );
    Ok((old, new))
}
pub fn log_entries(repo: &Repo, name: &str) -> Result<Vec<Vec<u8>>> {
    crate::repo::reference(name)?;
    let bytes = repo
        .read(&format!("logs/{name}"), LIMIT)?
        .unwrap_or_default();
    let lines: Vec<_> = bytes
        .split_inclusive(|b| *b == b'\n')
        .map(Vec::from)
        .collect();
    ensure!(lines.len() <= 10000, "reflog entry limit exceeded");
    for line in &lines {
        ensure!(
            log_ids(line)?.0.format() == repo.format,
            "reflog object format mismatch"
        );
    }
    Ok(lines)
}
pub struct Log<'a> {
    repo: &'a Repo,
    path: String,
    original: Option<Vec<u8>>,
    bytes: Vec<u8>,
}
impl Log<'_> {
    pub fn commit(self) -> Result<()> {
        let (dir, name) = self.repo.parent(&self.path, true)?;
        let mut lock = Pending::new(dir, format!("{name}.lock"))?;
        ensure!(
            self.repo.read(&self.path, LIMIT)? == self.original,
            "reflog changed concurrently"
        );
        lock.file.write_all(&self.bytes)?;
        lock.commit(&name)
    }
}
pub fn prepare_log<'a>(
    repo: &'a Repo,
    name: &str,
    old: Option<Oid>,
    new: Oid,
    message: &str,
) -> Result<Log<'a>> {
    let mut bytes = log_entries(repo, name)?.concat();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let message = compat::text(message).replace(['\n', '\r', '\t'], " ");
    bytes.extend_from_slice(
        format!(
            "{} {new} safe-git <safe-git@localhost> {now} +0000\t{message}\n",
            old.unwrap_or(repo.format.zero())
        )
        .as_bytes(),
    );
    ensure!(bytes.len() <= LIMIT, "reflog exceeds limit");
    let path = format!("logs/{name}");
    repo.parent(&path, true)?;
    ensure!(!repo.exists(&format!("{path}.lock"))?, "reflog is locked");
    let original = repo.read(&path, LIMIT)?;
    Ok(Log {
        repo,
        path,
        original,
        bytes,
    })
}
pub fn logs<'a>(
    repo: &'a Repo,
    name: &str,
    old: Option<Oid>,
    new: Oid,
    message: &str,
) -> Result<Vec<Log<'a>>> {
    let mut logs = vec![prepare_log(repo, name, old, new, message)?];
    if name != "HEAD" && repo.head()? == name {
        logs.push(prepare_log(repo, "HEAD", old, new, message)?);
    }
    Ok(logs)
}
pub fn finish_logs(logs: Vec<Log<'_>>) -> Result<()> {
    for log in logs {
        log.commit()?;
    }
    Ok(())
}
pub fn previous(repo: &Repo) -> Result<String> {
    for line in log_entries(repo, "HEAD")?.iter().rev() {
        let text = std::str::from_utf8(line)?;
        if let Some((_, message)) = text.split_once("\tcheckout: moving from ")
            && let Some((from, _)) = message.split_once(" to ")
        {
            return Ok(from.to_owned());
        }
    }
    bail!("no previous checkout recorded")
}
pub fn reflog(repo: &Repo, args: &[String]) -> Result<()> {
    if args.first().is_some_and(|a| a == "expire") {
        return crate::maintenance::expire_reflogs(repo, &args[1..]);
    }
    let mut reference = "HEAD";
    let mut count = 10000;
    for arg in args {
        if arg == "show" || arg == "--oneline" {
            continue;
        }
        if let Some(n) = arg
            .strip_prefix('-')
            .filter(|v| v.bytes().all(|b| b.is_ascii_digit()))
        {
            count = n.parse()?;
        } else if arg.starts_with('-') {
            bail!("unsupported reflog option {arg}");
        } else {
            reference = arg;
        }
    }
    let name = if reference == "HEAD" || reference.starts_with("refs/") {
        reference.to_owned()
    } else {
        format!("refs/heads/{}", crate::repo::branch(reference)?)
    };
    for (i, line) in log_entries(repo, &name)?
        .iter()
        .rev()
        .take(count)
        .enumerate()
    {
        let id = log_ids(line)?.1;
        let message = line.splitn(2, |b| *b == b'\t').nth(1).unwrap();
        println!(
            "{} {}@{{{i}}}: {}",
            crate::output::short(id),
            compat::text(reference),
            compat::text(String::from_utf8_lossy(message).trim_end())
        );
    }
    Ok(())
}
pub fn track(repo: &Repo, local: &str, remote_ref: &str) -> Result<()> {
    let short = remote_ref
        .strip_prefix("refs/remotes/")
        .unwrap_or(remote_ref);
    let remote = short
        .split_once('/')
        .context("tracking requires REMOTE/BRANCH")?
        .0;
    let mut binding = trust::named(repo, remote)?.context("remote is not registered")?;
    let target = short
        .strip_prefix(&format!("{}/", binding.remote))
        .context("tracking requires a branch of the host-registered remote")?;
    crate::repo::branch(local)?;
    crate::repo::branch(target)?;
    ensure!(
        repo.read_ref(&crate::repo::tracking(&binding.remote, target)?)?
            .is_some(),
        "remote branch does not exist"
    );
    binding.upstreams.insert(local.into(), target.into());
    trust::save(repo, &binding)
}
pub fn branch(cli: &Cli, repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    let mut mode = "list";
    let mut force = false;
    let mut remote = false;
    let mut all = false;
    let mut names = Vec::new();
    let mut upstream = None;
    let mut items = args.iter();
    while let Some(arg) = items.next() {
        match arg.as_str() {
            "--list" | "-l" => {}
            "-a" | "--all" => all = true,
            "-r" | "--remotes" => remote = true,
            "-d" | "--delete" => mode = "delete",
            "-D" => {
                mode = "delete";
                force = true;
            }
            "-m" | "--move" => mode = "move",
            "-M" => {
                mode = "move";
                force = true;
            }
            "-f" | "--force" => force = true,
            "-u" | "--set-upstream-to" => {
                upstream = Some(items.next().context("missing upstream")?.as_str())
            }
            a if a.starts_with("--set-upstream-to=") => upstream = Some(&a[18..]),
            a if a.starts_with('-') => bail!("unsupported branch option {a}"),
            _ => names.push(arg.as_str()),
        }
    }
    let head = repo.head()?;
    if let Some(upstream) = upstream {
        ensure!(names.len() <= 1, "too many branch names");
        return track(
            repo,
            names
                .first()
                .copied()
                .unwrap_or(head.strip_prefix("refs/heads/").context("detached HEAD")?),
            upstream,
        );
    }
    if names.is_empty() && mode == "list" {
        for (name, _) in repo.refs()? {
            if let Some(short) = name.strip_prefix("refs/heads/").filter(|_| !remote || all) {
                println!(
                    "{} {}",
                    if name == head { '*' } else { ' ' },
                    compat::text(short)
                );
            } else if let Some(short) = name.strip_prefix("refs/remotes/").filter(|_| remote || all)
            {
                println!(
                    "  {}{}",
                    if all { "remotes/" } else { "" },
                    compat::text(short)
                );
            }
        }
        return Ok(());
    }
    ensure!(
        !remote && !all,
        "remote branch mutation is unsupported; use push --delete for remote deletion"
    );
    match mode {
        "delete" => {
            ensure!(!names.is_empty(), "branch deletion requires names");
            let tip = repo.read_ref("HEAD")?.context("unborn HEAD")?;
            let mut refs = Vec::new();
            for name in names {
                let name = format!("refs/heads/{}", crate::repo::branch(name)?);
                ensure!(name != head, "cannot delete the current branch");
                crate::linked::available(repo, &name)?;
                let old = repo.read_ref(&name)?.context("branch does not exist")?;
                ensure!(
                    force || ancestor(store, old, tip)?,
                    "branch is not fully merged; use -D to delete it"
                );
                refs.push((name, old));
            }
            for (name, old) in refs {
                repo.delete_ref(&name, old)?;
                repo.remove(&format!("logs/{name}"))?;
                if let Some(mut b) = trust::get(repo)? {
                    b.upstreams.remove(name.trim_start_matches("refs/heads/"));
                    trust::save(repo, &b)?;
                }
            }
        }
        "move" => {
            ensure!(
                (1..=2).contains(&names.len()),
                "branch -m accepts [old] new"
            );
            let from = if names.len() == 2 {
                crate::repo::branch(names[0])?
            } else {
                head.strip_prefix("refs/heads/")
                    .context("detached HEAD")?
                    .to_owned()
            };
            let to = crate::repo::branch(names[names.len() - 1])?;
            if from == to {
                return Ok(());
            }
            let old_name = format!("refs/heads/{from}");
            let new_name = format!("refs/heads/{to}");
            ensure!(new_name != head, "cannot overwrite the current branch");
            crate::linked::available(repo, &old_name)?;
            crate::linked::available(repo, &new_name)?;
            let old = repo.read_ref(&old_name)?.context("branch does not exist")?;
            let existing = repo.read_ref(&new_name)?;
            ensure!(force || existing.is_none(), "destination branch exists");
            let log = prepare_log(
                repo,
                &new_name,
                existing,
                old,
                &format!("branch: renamed {from} to {to}"),
            )?;
            repo.update_ref(&new_name, existing, old)?;
            if head == old_name {
                let mut lock = Pending::new(repo.root.try_clone()?, "HEAD.lock".into())?;
                lock.file
                    .write_all(format!("ref: {new_name}\n").as_bytes())?;
                lock.commit("HEAD")?;
            }
            repo.delete_ref(&old_name, old)?;
            log.commit()?;
            if let Some(mut b) = trust::get(repo)? {
                if let Some(up) = b.upstreams.remove(&from) {
                    b.upstreams.insert(to, up);
                }
                trust::save(repo, &b)?;
            }
        }
        _ => {
            ensure!(
                (1..=2).contains(&names.len()),
                "branch accepts name and optional start point"
            );
            let name = format!("refs/heads/{}", crate::repo::branch(names[0])?);
            crate::linked::available(repo, &name)?;
            let old = repo.read_ref(&name)?;
            ensure!(
                old.is_none() || (force && name != head),
                "branch exists or is checked out"
            );
            let tip = revision::commit(repo, store, names.get(1).copied().unwrap_or("HEAD"))?;
            let log = prepare_log(repo, &name, old, tip, "branch: created")?;
            repo.update_ref(&name, old, tip)?;
            log.commit()?;
        }
    }
    let _ = cli;
    Ok(())
}
pub fn reset(repo: &Repo, store: &mut Store<'_>, args: &[String], quiet: bool) -> Result<()> {
    let mut mode = "--mixed";
    let mut value = "HEAD";
    for arg in args {
        match arg.as_str() {
            "--soft" | "--mixed" | "--hard" => mode = arg,
            "-q" | "--quiet" => {}
            a if a.starts_with('-') => bail!("unsupported reset option {a}"),
            _ => value = arg,
        }
    }
    let id = revision::commit(repo, store, value)?;
    if mode == "--hard" {
        porcelain::reset(repo, store, id, quiet)?;
        repo.remove("safe-git-sequence")?;
        return Ok(());
    }
    ensure!(
        !repo.exists("safe-git-sequence")?,
        "operation in progress; use reset --hard or abort it"
    );
    let head = repo.head()?;
    let old = repo.read_ref(&head)?;
    let reference = repo.prepare_ref(&head, old)?;
    let _head = if head != "HEAD" {
        Some(Pending::new(repo.root.try_clone()?, "HEAD.lock".into())?)
    } else {
        None
    };
    let logs = logs(repo, &head, old, id, &format!("reset: moving to {value}"))?;
    if mode == "--mixed" {
        let mut lock = Pending::new(repo.root.try_clone()?, "index.lock".into())?;
        let mut entries = porcelain::tree(store, id)?;
        crate::sparse::mark(repo, &mut entries)?;
        lock.file
            .write_all(&index::encode_for(&entries, repo.format)?)?;
        lock.commit("index")?;
    }
    if let Some(old) = old {
        repo.atomic("ORIG_HEAD", format!("{old}\n").as_bytes())?;
    }
    reference.commit(id)?;
    finish_logs(logs)
}
