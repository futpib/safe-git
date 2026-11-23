//! The additional Git command surface exercised by paru, makepkg and common pkgver functions.
use crate::{
    Cli, config, network,
    object::{Kind, Oid, ReadObjects, ancestor},
    porcelain,
    repo::{Repo, Store},
    revision, trust,
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    io::Write,
    os::fd::AsRawFd,
};
#[derive(Debug)]
pub struct Exit(pub i32);
impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit {}", self.0)
    }
}
impl std::error::Error for Exit {}
fn predicate(value: bool) -> Result<()> {
    if value { Ok(()) } else { Err(Exit(1).into()) }
}
pub fn text(value: &str) -> String {
    value
        .chars()
        .flat_map(|c| {
            if (c.is_control() && c != '\n' && c != '\t')
                || matches!(c,'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}')
            {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}
pub fn diff_budget(a: &[u8], b: &[u8]) -> Result<()> {
    ensure!(
        a.len() + b.len() <= 16 * 1024 * 1024,
        "diff byte budget exceeded"
    );
    let a: Vec<_> = a.split_inclusive(|b| *b == b'\n').collect();
    let b: Vec<_> = b.split_inclusive(|b| *b == b'\n').collect();
    ensure!(a.len() + b.len() <= 200_000, "diff line budget exceeded");
    let prefix = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
    let a = &a[prefix..];
    let b = &b[prefix..];
    let suffix = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    ensure!(
        (a.len() - suffix).saturating_mul(b.len() - suffix) <= 8_000_000,
        "diff comparison budget exceeded"
    );
    Ok(())
}
fn glob(pattern: &str, value: &str) -> bool {
    if pattern.len() > 4096 || value.len() > 4096 {
        return false;
    }
    let mut row = vec![false; value.len() + 1];
    row[0] = true;
    for p in pattern.bytes() {
        let mut next = vec![false; row.len()];
        if p == b'*' {
            next[0] = row[0];
        }
        for (i, b) in value.bytes().enumerate() {
            next[i + 1] = if p == b'*' {
                row[i + 1] || next[i]
            } else {
                row[i] && (p == b'?' || p == b)
            };
        }
        row = next;
    }
    row[value.len()]
}
fn args<'a>(items: &'a [String], allowed: &[&str]) -> Result<Vec<&'a str>> {
    let mut positional = Vec::new();
    let mut end = false;
    for arg in items {
        if arg == "--" && !end {
            end = true;
            continue;
        }
        if !end && arg.starts_with('-') {
            ensure!(
                allowed
                    .iter()
                    .any(|p| arg == p || (p.ends_with('=') && arg.starts_with(p))),
                "unsupported option {arg}"
            );
        } else {
            positional.push(arg.as_str());
        }
    }
    Ok(positional)
}
fn flag(items: &[String], name: &str) -> bool {
    items.iter().any(|a| a == name)
}
fn option<'a>(items: &'a [String], prefix: &str) -> Option<&'a str> {
    items.iter().find_map(|a| a.strip_prefix(prefix))
}
fn one<'a>(pos: &'a [&'a str], default: &'a str) -> Result<&'a str> {
    ensure!(pos.len() <= 1, "too many revisions");
    Ok(pos.first().copied().unwrap_or(default))
}
fn ready(repo: &Repo) -> Result<()> {
    ensure!(
        !repo.exists("safe-git-checkout")?,
        "interrupted checkout; run safe-git recover"
    );
    Ok(())
}

pub async fn run(cli: &Cli, command: &[String]) -> Result<()> {
    let name = command.first().context("missing command")?.as_str();
    let items = &command[1..];
    if name == "config" && flag(items, "--global") {
        return config::global(items);
    }
    if name == "ls-remote" {
        let pos = args(
            items,
            &[
                "--heads",
                "--tags",
                "--refs",
                "--exit-code",
                "-q",
                "--quiet",
                "--symref",
            ],
        )?;
        let url = pos
            .first()
            .copied()
            .or(cli.remote_url.as_deref())
            .context("ls-remote requires an explicit URL")?;
        let ad = network::discover(
            &network::normalize_from(url, &cli.repository)?,
            &cli.auth()?,
        )
        .await?;
        let refs: BTreeMap<_, _> = ad.refs.iter().collect();
        let mut count = 0;
        for (name, id) in refs {
            if (flag(items, "--heads") || flag(items, "--tags"))
                && !(flag(items, "--heads") && name.starts_with("refs/heads/")
                    || flag(items, "--tags") && name.starts_with("refs/tags/"))
            {
                continue;
            }
            if flag(items, "--refs") && (name == "HEAD" || name.ends_with("^{}")) {
                continue;
            }
            if pos.len() > 1
                && !pos[1..].iter().any(|p| {
                    glob(p, name)
                        || name.split('/').enumerate().any(|(i, _)| {
                            glob(p, &name.split('/').skip(i).collect::<Vec<_>>().join("/"))
                        })
                })
            {
                continue;
            }
            if name == "HEAD"
                && flag(items, "--symref")
                && let Ok((branch, Some(_))) = ad.clone_branch(None)
            {
                println!("ref: refs/heads/{branch}\tHEAD");
            }
            println!("{id}\t{name}");
            count += 1;
        }
        if flag(items, "--exit-code") && count == 0 {
            return Err(Exit(2).into());
        }
        return Ok(());
    }
    ensure!(
        [
            "config",
            "rev-parse",
            "symbolic-ref",
            "update-ref",
            "merge-base",
            "reset",
            "checkout",
            "rebase",
            "diff",
            "log",
            "commit",
            "tag",
            "rev-list",
            "describe",
            "archive",
            "cherry-pick",
            "revert",
            "branch",
            "reflog",
            "status",
            "add",
            "switch",
            "restore",
            "stash",
            "show",
            "blame",
            "rm",
            "mv",
            "clean",
            "remote",
            "worktree",
            "bisect",
            "submodule",
            "grep",
            "apply",
            "am",
            "format-patch",
            "sparse-checkout"
        ]
        .contains(&name),
        "unsupported Git command {name}; no external fallback is implemented"
    );
    let repo = Repo::open(&cli.repository)?;
    ready(&repo)?;
    if matches!(
        name,
        "checkout" | "branch" | "stash" | "switch" | "update-ref" | "symbolic-ref"
    ) {
        ensure!(
            !repo.exists("safe-git-sequence")?,
            "operation in progress; continue or abort it first"
        );
    }
    ensure!(
        !repo.exists("safe-git-am")?
            || matches!(
                name,
                "am" | "add"
                    | "restore"
                    | "apply"
                    | "status"
                    | "diff"
                    | "log"
                    | "show"
                    | "grep"
                    | "rev-parse"
            ),
        "am in progress; continue or abort it first"
    );
    let mut store = Store::new(&repo);
    match name {
        "sparse-checkout" => crate::sparse::run(cli, &repo, &mut store, items),
        "grep" => crate::search::grep(cli, &repo, &mut store, items),
        "apply" | "am" | "format-patch" => crate::mail::run(cli, &repo, &mut store, name, items),
        "show" | "blame" => crate::inspect::run(cli, &repo, &mut store, name, items),
        "rm" | "mv" | "clean" => crate::files::run(cli, &repo, &mut store, name, items),
        "remote" => crate::remotes::run(cli, &repo, &mut store, items).await,
        "worktree" => crate::linked::run(cli, &repo, &mut store, items),
        "bisect" => crate::bisect::run(cli, &repo, &mut store, items),
        "submodule" => crate::submodule::run(cli, &repo, &mut store, items).await,
        "branch" => crate::history::branch(cli, &repo, &mut store, items),
        "reflog" => crate::history::reflog(&repo, items),
        "status" | "add" | "switch" | "restore" | "stash" => {
            crate::workflow::run(cli, &repo, &mut store, name, items)
        }
        "config" => {
            let pos = args(items, &["--get", "--local"])?;
            ensure!(
                (1..=2).contains(&pos.len()),
                "config requires a key and optional value"
            );
            if pos.len() == 2 {
                ensure!(!flag(items, "--get"), "--get takes one key");
                return config::set(&repo, pos[0], pos[1], &cli.repository);
            }
            if let Some(value) = config::get(&repo, &cli.config, pos[0])? {
                println!("{}", text(&value));
                Ok(())
            } else {
                Err(Exit(1).into())
            }
        }
        "rev-parse" => {
            let pos = args(
                items,
                &[
                    "--verify",
                    "--quiet",
                    "-q",
                    "--short",
                    "--short=",
                    "--absolute-git-dir",
                    "--git-dir",
                    "--git-path",
                    "--is-bare-repository",
                    "--is-inside-work-tree",
                    "--show-toplevel",
                    "--abbrev-ref",
                ],
            )?;
            if flag(items, "--git-path") {
                ensure!(
                    pos.len() == 1
                        && pos[0].len() <= 4096
                        && !pos[0].chars().any(char::is_control)
                        && pos[0].split('/').all(|p| !matches!(p, "" | "." | "..")),
                    "invalid internal Git path"
                );
                println!(
                    "{}{}",
                    if repo.worktree.is_some() { ".git/" } else { "" },
                    pos[0]
                );
                return Ok(());
            }
            if flag(items, "--absolute-git-dir") {
                println!(
                    "{}",
                    std::fs::read_link(format!("/proc/self/fd/{}", repo.root.as_raw_fd()))?
                        .display()
                );
                return Ok(());
            }
            if flag(items, "--git-dir") {
                println!("{}", if repo.worktree.is_some() { ".git" } else { "." });
                return Ok(());
            }
            if flag(items, "--show-toplevel") {
                ensure!(repo.worktree.is_some(), "requires a working tree");
                println!("{}", cli.repository.canonicalize()?.display());
                return Ok(());
            }
            if flag(items, "--is-bare-repository") {
                println!("{}", repo.worktree.is_none());
                return Ok(());
            }
            if flag(items, "--is-inside-work-tree") {
                println!("{}", repo.worktree.is_some());
                return Ok(());
            }
            ensure!(
                !pos.is_empty() && (!flag(items, "--verify") || pos.len() == 1),
                "rev-parse requires a revision"
            );
            for value in pos {
                if flag(items, "--abbrev-ref") && value == "HEAD" {
                    println!(
                        "{}",
                        repo.head()?.strip_prefix("refs/heads/").unwrap_or("HEAD")
                    );
                    continue;
                }
                let id = match revision::resolve(&repo, &mut store, value) {
                    Ok(id) => id,
                    Err(_) if flag(items, "--quiet") || flag(items, "-q") => {
                        return Err(Exit(1).into());
                    }
                    Err(e) => return Err(e),
                };
                let length = if flag(items, "--short") || option(items, "--short=").is_some() {
                    option(items, "--short=").unwrap_or("7").parse::<usize>()?
                } else {
                    40
                };
                ensure!((4..=40).contains(&length), "invalid abbreviation length");
                println!("{}", abbrev(&mut store, id, length)?);
            }
            Ok(())
        }
        "symbolic-ref" => {
            let pos = args(items, &["-q", "--quiet", "--short"])?;
            ensure!(pos == ["HEAD"], "only reading symbolic HEAD is supported");
            let head = repo.head()?;
            if head == "HEAD" {
                return Err(Exit(1).into());
            }
            println!(
                "{}",
                if flag(items, "--short") {
                    head.trim_start_matches("refs/heads/")
                } else {
                    &head
                }
            );
            Ok(())
        }
        "update-ref" => {
            let pos = args(items, &["-d"])?;
            ensure!(
                (2..=3).contains(&pos.len()) || flag(items, "-d") && pos.len() == 1,
                "update-ref requires name and value"
            );
            let name = if pos[0] == "HEAD" {
                repo.head()?
            } else {
                pos[0].to_owned()
            };
            crate::repo::reference(&name)?;
            ensure!(
                !name.starts_with("refs/replace/"),
                "replacement refs are unsupported"
            );
            let old = repo.read_ref(&name)?;
            if flag(items, "-d") {
                ensure!(
                    pos.len() <= 2,
                    "update-ref -d accepts name and optional old value"
                );
                if pos.len() == 2 {
                    ensure!(
                        old.unwrap_or(Oid::ZERO) == Oid::parse(pos[1].as_bytes())?,
                        "ref changed concurrently"
                    );
                }
                if let Some(old) = old {
                    repo.delete_ref(&name, old)?;
                }
                return Ok(());
            }
            if pos.len() == 3 {
                let expected = Oid::parse(pos[2].as_bytes())?;
                ensure!(
                    old.unwrap_or(Oid::ZERO) == expected,
                    "ref changed concurrently"
                );
            }
            let id = revision::resolve(&repo, &mut store, pos[1])?;
            store.get(id)?;
            repo.update_ref(&name, old, id)
        }
        "merge-base" => {
            let pos = args(items, &["--is-ancestor"])?;
            ensure!(pos.len() == 2, "merge-base requires two revisions");
            let a = revision::commit(&repo, &mut store, pos[0])?;
            let b = revision::commit(&repo, &mut store, pos[1])?;
            if flag(items, "--is-ancestor") {
                predicate(ancestor(&mut store, a, b)?)
            } else {
                println!("{}", revision::merge_base(&mut store, a, b)?);
                Ok(())
            }
        }
        "reset" if items.iter().any(|a| a == "--") => {
            crate::workflow::legacy_restore(cli, &repo, &mut store, items, true)
        }
        "reset" => crate::history::reset(&repo, &mut store, items, cli.quiet),
        "checkout"
            if items
                .iter()
                .position(|a| a == "--")
                .is_some_and(|i| i + 1 < items.len()) =>
        {
            crate::workflow::legacy_restore(cli, &repo, &mut store, items, false)
        }
        "checkout" => {
            let mut filtered = Vec::new();
            let mut branch = None;
            let mut i = 0;
            while i < items.len() {
                if items[i] == "-B" || items[i] == "-b" {
                    ensure!(branch.is_none(), "duplicate branch option");
                    i += 1;
                    branch = Some(items.get(i).context("branch option needs a name")?.as_str());
                    if items[i - 1] == "-b" {
                        ensure!(
                            repo.read_ref(&format!(
                                "refs/heads/{}",
                                crate::repo::branch(branch.unwrap())?
                            ))?
                            .is_none(),
                            "branch already exists"
                        );
                    }
                } else {
                    filtered.push(items[i].clone());
                }
                i += 1;
            }
            let pos = args(&filtered, &["--force", "-f", "--no-track", "-q", "--quiet"])?;
            let value = one(&pos, "HEAD")?;
            porcelain::checkout(
                &repo,
                &mut store,
                value,
                branch,
                flag(items, "--force") || flag(items, "-f"),
                cli.quiet || flag(items, "-q") || flag(items, "--quiet"),
            )
        }
        "rebase" | "cherry-pick" | "revert" => {
            crate::sequence::run(cli, &repo, &mut store, name, items)
        }
        "commit" => {
            let mut message: Option<String> = None;
            let mut fixup = None;
            let mut amend = false;
            let mut no_edit = false;
            let mut all = false;
            let mut quiet = cli.quiet;
            let mut allow_empty = false;
            let mut author = None;
            let mut names = Vec::new();
            let mut end = false;
            let mut i = 0;
            while i < items.len() {
                if end {
                    names.push(items[i].clone());
                    i += 1;
                    continue;
                }
                match items[i].as_str() {
                    "--" => end = true,
                    "--allow-empty" => allow_empty = true,
                    "--author" => {
                        i += 1;
                        author = Some(items.get(i).context("missing author")?.clone());
                    }
                    a if a.starts_with("--author=") => author = Some(a[9..].into()),
                    "-am" | "-m" | "--message" => {
                        all |= items[i] == "-am";
                        i += 1;
                        message = Some(items.get(i).context("missing commit message")?.clone());
                    }
                    "-F" | "--file" => {
                        i += 1;
                        message = Some(String::from_utf8(crate::mail::read_input(
                            cli,
                            items.get(i).context("missing message file")?,
                        )?)?);
                    }
                    a if a.starts_with("--file=") => {
                        message = Some(String::from_utf8(crate::mail::read_input(cli, &a[7..])?)?)
                    }
                    "--fixup" => {
                        i += 1;
                        fixup = Some(items.get(i).context("missing fixup revision")?.as_str());
                    }
                    a if a.starts_with("--fixup=") => fixup = Some(&a[8..]),
                    "--amend" => amend = true,
                    "--no-edit" => no_edit = true,
                    "-a" | "--all" => all = true,
                    "-q" | "--quiet" => quiet = true,
                    other if other.starts_with('-') => {
                        anyhow::bail!("unsupported commit option {other}")
                    }
                    _ => names.push(items[i].clone()),
                }
                i += 1;
            }
            if let Some(value) = fixup {
                ensure!(!amend, "--fixup and --amend are incompatible");
                let id = revision::commit(&repo, &mut store, value)?;
                let subject = porcelain::subject(&store.get(id)?.data);
                message = Some(format!(
                    "fixup! {subject}\n{}",
                    message.map(|m| format!("\n{m}")).unwrap_or_default()
                ));
            }
            if amend {
                crate::sequence::amend_allowed(&repo)?;
            }
            ensure!(
                !repo.exists("safe-git-sequence")? || (names.is_empty() && author.is_none()),
                "path/author overrides cannot change an in-progress sequence; finish it first"
            );
            if !amend
                && let Some(message) = message.as_deref()
                && crate::sequence::commit_pending(cli, &repo, &mut store, message)?
            {
                return Ok(());
            }
            let previous_message = if amend && no_edit && message.is_none() {
                Some(String::from_utf8(
                    porcelain::message(
                        &store
                            .get(repo.read_ref("HEAD")?.context("unborn HEAD")?)?
                            .data,
                    )?
                    .to_vec(),
                )?)
            } else {
                None
            };
            porcelain::commit(
                &repo,
                &mut store,
                &cli.config,
                message.as_deref().or(previous_message.as_deref()).context(
                    "commit requires -m or --amend --no-edit; editors are not implemented",
                )?,
                porcelain::CommitOptions {
                    all,
                    quiet,
                    amend,
                    allow_empty,
                    author,
                    paths: if names.is_empty() {
                        None
                    } else {
                        Some(crate::workflow::paths(&names, &cli.prefix)?)
                    },
                },
            )
        }
        "diff" => diff(&repo, &mut store, items),
        "log" | "rev-list" => history(&repo, &mut store, items, name == "rev-list", &cli.prefix),
        "tag" if crate::tags::is_mutation(items) => crate::tags::run(cli, &repo, &mut store, items),
        "tag" => {
            let pos = args(items, &["-l", "--list", "--format="])?;
            ensure!(
                flag(items, "-l") || flag(items, "--list") || items.is_empty(),
                "only tag listing is implemented"
            );
            let format = option(items, "--format=").unwrap_or("%(refname:short)");
            ensure!(
                ["%(tag)", "%(refname:short)", "%(objectname)"].contains(&format),
                "unsupported tag format"
            );
            for (name, id) in repo.refs()? {
                if let Some(tag) = name.strip_prefix("refs/tags/") {
                    if !pos.is_empty() && !pos.iter().any(|p| glob(p, tag)) {
                        continue;
                    }
                    let out = match format {
                        "%(tag)" => {
                            let obj = store.get(id)?;
                            if obj.kind == Kind::Tag {
                                String::from_utf8_lossy(
                                    obj.data
                                        .split(|b| *b == b'\n')
                                        .find_map(|l| l.strip_prefix(b"tag "))
                                        .context("tag has no name")?,
                                )
                                .into_owned()
                            } else {
                                String::new()
                            }
                        }
                        "%(objectname)" => id.to_string(),
                        _ => tag.to_owned(),
                    };
                    println!("{}", text(&out));
                }
            }
            Ok(())
        }
        "describe" => describe(&repo, &mut store, items),
        "archive" => {
            let mut filtered = Vec::new();
            let mut format = "tar";
            let mut prefix = "";
            let mut i = 0;
            while i < items.len() {
                if items[i] == "--format" || items[i] == "--prefix" {
                    let key = &items[i];
                    i += 1;
                    let value = items.get(i).context("missing archive option")?;
                    if key == "--format" {
                        format = value;
                    } else {
                        prefix = value;
                    }
                } else if let Some(v) = items[i].strip_prefix("--format=") {
                    format = v;
                } else if let Some(v) = items[i].strip_prefix("--prefix=") {
                    prefix = v;
                } else {
                    filtered.push(items[i].clone());
                }
                i += 1;
            }
            let pos = args(&filtered, &[])?;
            ensure!(
                format == "tar" && pos.len() == 1,
                "archive supports --format tar and one revision"
            );
            let id = revision::resolve(&repo, &mut store, pos[0])?;
            let bytes = crate::archive::tar(&mut store, id, prefix)?;
            std::io::stdout().lock().write_all(&bytes)?;
            Ok(())
        }
        _ => unreachable!(),
    }
}
pub async fn fetch_binding(
    cli: &Cli,
    repo: &Repo,
    store: &mut Store<'_>,
    prune: bool,
    binding: &trust::Binding,
) -> Result<()> {
    ensure!(
        !binding.bare || repo.worktree.is_none(),
        "registered repository mode changed"
    );
    let ad = network::discover(&binding.url, &cli.auth()?).await?;
    let selected: HashMap<_, _> = ad
        .refs
        .iter()
        .filter(|(name, _)| {
            !name.ends_with("^{}")
                && if binding.mirror {
                    name.starts_with("refs/")
                } else if binding.single {
                    **name == format!("refs/heads/{}", binding.branch)
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
    let existing = repo.refs()?;
    let mut haves: Vec<_> = existing
        .iter()
        .filter(|(n, _)| {
            n.starts_with("refs/heads/") || n.starts_with("refs/remotes/") && !n.ends_with("/HEAD")
        })
        .map(|(_, id)| *id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(32)
        .collect();
    for id in &haves {
        store.get(*id)?.commit()?;
    }
    if !store.shallow.is_empty() {
        haves.clear();
    }
    let options = crate::fetch_options(cli, store, binding.filter.clone());
    let boundary = crate::partial::boundary(store, &haves, &options)?;
    let fetched =
        network::fetch_options(&binding.url, &cli.auth()?, &selected, &haves, &options).await?;
    if binding.filter.is_some() {
        crate::partial::store_pack(repo, &fetched.objects)?;
    }
    store.shallow = fetched.shallow;
    store.add(fetched.objects)?;
    let seen = crate::partial::persist(
        repo,
        store,
        &selected.values().copied().collect::<Vec<_>>(),
        binding.filter.is_some(),
        &boundary,
    )?;
    crate::shallow::validate(store, &seen, &options.shallow)?;
    crate::shallow::save(repo, &store.shallow)?;
    let mut updates = BTreeMap::new();
    for (name, id) in &selected {
        if name.starts_with("refs/heads/") {
            store.get(*id)?.commit()?;
        }
        let dest = if binding.mirror || binding.bare || name.starts_with("refs/tags/") {
            name.clone()
        } else {
            crate::repo::tracking(&binding.remote, name.strip_prefix("refs/heads/").unwrap())?
        };
        let old = repo.read_ref(&dest)?;
        ensure!(
            binding.mirror || !dest.starts_with("refs/tags/") || old.is_none_or(|o| o == *id),
            "fetch would clobber existing tag {dest}"
        );
        updates.insert(dest, (old, *id));
    }
    for (dest, (old, id)) in &updates {
        repo.update_ref(dest, *old, *id)?;
        if !cli.quiet && *old != Some(*id) {
            let branch = dest.strip_prefix(&format!("refs/remotes/{}/", binding.remote));
            if let Some(branch) = branch {
                let ff = old.is_none() || ancestor(store, old.unwrap(), *id)?;
                eprint!(
                    "{}",
                    crate::output::fetch(&binding.url, branch, &binding.remote, *old, *id, ff)
                );
            } else {
                eprintln!(" * [updated] {dest}");
            }
        }
    }
    if prune {
        for (name, id) in existing {
            if !name.ends_with("/HEAD")
                && (binding.mirror
                    || name.starts_with(&format!("refs/remotes/{}/", binding.remote)))
                && !updates.contains_key(&name)
            {
                repo.delete_ref(&name, id)?;
            }
        }
    }
    if let Some(id) = ad.branch(&binding.branch) {
        repo.atomic(
            "FETCH_HEAD",
            format!("{id}\t\tbranch '{}' of {}\n", binding.branch, binding.url).as_bytes(),
        )?;
    }
    Ok(())
}
fn abbrev(store: &mut Store<'_>, id: Oid, length: usize) -> Result<String> {
    if length == 40 {
        return Ok(id.to_string());
    }
    let ids = store.ids()?;
    let full = id.to_string();
    let mut length = length;
    while length < 40
        && ids
            .iter()
            .any(|other| *other != id && other.to_string().starts_with(&full[..length]))
    {
        length += 1;
    }
    Ok(full[..length].to_owned())
}
pub(crate) fn diff(repo: &Repo, store: &mut Store<'_>, items: &[String]) -> Result<()> {
    let separator = items.iter().position(|a| a == "--").unwrap_or(items.len());
    let pos = args(
        &items[..separator],
        &[
            "--stat",
            "--patch",
            "-p",
            "--cached",
            "--staged",
            "--exit-code",
            "--quiet",
            "--color=",
            "--no-color",
            "--no-ext-diff",
            "--name-only",
            "--name-status",
            "--check",
        ],
    )?;
    let staged = porcelain::index(repo)?;
    let (mut before, mut after) = if pos.is_empty() {
        if flag(items, "--cached") || flag(items, "--staged") {
            (
                crate::index::tree(store, repo.read_ref("HEAD")?)?,
                staged
                    .into_iter()
                    .filter(|(_, e)| e.flags & crate::index::INTENT == 0)
                    .collect(),
            )
        } else {
            let actual = crate::checkout::snapshot(repo, &staged)?;
            (
                staged
                    .into_iter()
                    .filter(|(_, e)| e.flags & crate::index::INTENT == 0)
                    .collect(),
                actual,
            )
        }
    } else if pos.len() == 1 && pos[0].contains("..") {
        let (a, b) = pos[0].split_once("..").unwrap();
        let symmetric = b.starts_with('.');
        let b = b.strip_prefix('.').unwrap_or(b);
        let mut a = revision::resolve(repo, store, if a.is_empty() { "HEAD" } else { a })?;
        let b = revision::resolve(repo, store, if b.is_empty() { "HEAD" } else { b })?;
        if symmetric {
            a = revision::merge_base(store, a, b)?;
        }
        (porcelain::tree(store, a)?, porcelain::tree(store, b)?)
    } else if pos.len() == 2 {
        let a = revision::resolve(repo, store, pos[0])?;
        let b = revision::resolve(repo, store, pos[1])?;
        (porcelain::tree(store, a)?, porcelain::tree(store, b)?)
    } else {
        ensure!(pos.len() == 1, "diff accepts up to two revisions");
        let a = revision::resolve(repo, store, pos[0])?;
        (
            porcelain::tree(store, a)?,
            if flag(items, "--cached") || flag(items, "--staged") {
                staged
            } else {
                crate::checkout::snapshot(repo, &staged)?
            },
        )
    };
    before.retain(|_, e| e.flags & crate::index::INTENT == 0);
    after.retain(|_, e| e.flags & crate::index::INTENT == 0);
    if separator < items.len() {
        let paths = &items[separator + 1..];
        let keep = |path: &Vec<u8>, _: &mut crate::index::Entry| {
            let name = String::from_utf8_lossy(path);
            let included = paths
                .iter()
                .filter(|p| !p.starts_with(":!") && !p.starts_with(":(exclude)"))
                .collect::<Vec<_>>();
            (included.is_empty() || included.iter().any(|p| glob(p, &name)))
                && !paths
                    .iter()
                    .filter_map(|p| {
                        p.strip_prefix(":!")
                            .or_else(|| p.strip_prefix(":(exclude)"))
                    })
                    .any(|p| glob(p, &name))
        };
        before.retain(keep);
        after.retain(keep);
    }
    let changed = before != after;
    let mut whitespace_errors = false;
    ensure!(
        !flag(items, "--check") || !flag(items, "--exit-code"),
        "--check and --exit-code are incompatible"
    );
    if !flag(items, "--quiet") || flag(items, "--check") {
        if flag(items, "--stat") {
            let stat = crate::output::diffstat_entries(store, &before, &after)?;
            for line in stat.lines() {
                if !line.starts_with(" create mode ")
                    && !line.starts_with(" delete mode ")
                    && !line.starts_with(" mode change ")
                {
                    println!("{line}");
                }
            }
            if changed && (flag(items, "--patch") || flag(items, "-p")) {
                println!();
            }
        }
        let paths: BTreeSet<_> = before.keys().chain(after.keys()).collect();
        let mut budget = 16 * 1024 * 1024;
        let mut output = String::new();
        for path in paths {
            let a = before.get(path).copied();
            let b = after.get(path).copied();
            if a == b {
                continue;
            }
            if flag(items, "--check") {
                let old = crate::output::content(store, a, &mut budget)?;
                let new = crate::output::content(store, b, &mut budget)?;
                let (errors, diagnostics) = crate::mail::check_whitespace(path, &old, &new)?;
                whitespace_errors |= errors;
                output.push_str(&diagnostics);
                continue;
            }
            if flag(items, "--name-status") {
                let status = match (a, b) {
                    (None, _) => 'A',
                    (_, None) => 'D',
                    (Some(a), Some(b)) if a.mode & 0o170000 != b.mode & 0o170000 => 'T',
                    _ => 'M',
                };
                output.push_str(&format!("{status}\t{}\n", crate::output::quote(path)));
                continue;
            }
            if flag(items, "--name-only") {
                output.push_str(&format!("{}\n", crate::output::quote(path)));
                continue;
            }
            if flag(items, "--stat") && !flag(items, "--patch") && !flag(items, "-p") {
                continue;
            }
            let mut ap = b"a/".to_vec();
            ap.extend_from_slice(path);
            let ap = crate::output::quote(&ap);
            let mut bp = b"b/".to_vec();
            bp.extend_from_slice(path);
            let bp = crate::output::quote(&bp);
            output.push_str(&format!("diff --git {ap} {bp}\n"));
            match (a, b) {
                (None, Some(e)) => output.push_str(&format!("new file mode {:06o}\n", e.mode)),
                (Some(e), None) => output.push_str(&format!("deleted file mode {:06o}\n", e.mode)),
                (Some(a), Some(b)) if a.mode != b.mode => output.push_str(&format!(
                    "old mode {:06o}\nnew mode {:06o}\n",
                    a.mode, b.mode
                )),
                _ => {}
            }
            if a.map(|e| e.oid) == b.map(|e| e.oid) {
                continue;
            }
            let old = crate::output::content(store, a, &mut budget)?;
            let new = crate::output::content(store, b, &mut budget)?;
            let mode = if let Some((a, _)) = a.zip(b).filter(|(a, b)| a.mode == b.mode) {
                format!(" {:06o}", a.mode)
            } else {
                String::new()
            };
            output.push_str(&format!(
                "index {}..{}{mode}\n",
                crate::output::short(a.map(|e| e.oid).unwrap_or(Oid::ZERO)),
                crate::output::short(b.map(|e| e.oid).unwrap_or(Oid::ZERO))
            ));
            let ap = if a.is_none() { "/dev/null" } else { &ap };
            let bp = if b.is_none() { "/dev/null" } else { &bp };
            if old[..old.len().min(8000)].contains(&0) || new[..new.len().min(8000)].contains(&0) {
                output.push_str(&format!("Binary files {ap} and {bp} differ\n"));
                continue;
            }
            diff_budget(&old, &new)?;
            let patch = diffy::create_patch_bytes(&old, &new).to_bytes();
            let a_tab = if !ap.starts_with('"') && ap.contains(' ') {
                "\t"
            } else {
                ""
            };
            let b_tab = if !bp.starts_with('"') && bp.contains(' ') {
                "\t"
            } else {
                ""
            };
            output.push_str(&format!("--- {ap}{a_tab}\n+++ {bp}{b_tab}\n"));
            let mut lines = patch.split_inclusive(|b| *b == b'\n');
            lines.next();
            lines.next();
            for line in lines {
                output.push_str(&text(&String::from_utf8_lossy(line)));
            }
            ensure!(
                output.len() <= 64 * 1024 * 1024,
                "diff output exceeds limit"
            );
        }
        if !flag(items, "--quiet") {
            print!("{output}");
        }
    }
    if flag(items, "--check") {
        if whitespace_errors {
            Err(Exit(2).into())
        } else {
            Ok(())
        }
    } else if flag(items, "--exit-code") || flag(items, "--quiet") {
        predicate(!changed)
    } else {
        Ok(())
    }
}
fn timestamp(data: &[u8]) -> i64 {
    data.split(|b| *b == b'\n')
        .find_map(|l| l.strip_prefix(b"committer "))
        .and_then(|l| std::str::from_utf8(l).ok())
        .and_then(|s| s.rsplit(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}
fn timezone(zone: &str) -> Result<time::UtcOffset> {
    ensure!(
        zone.len() == 5
            && matches!(zone.as_bytes()[0], b'+' | b'-')
            && zone.as_bytes()[1..].iter().all(u8::is_ascii_digit),
        "invalid timezone"
    );
    let hour = zone[1..3].parse::<i32>()?;
    let minute = zone[3..].parse::<i32>()?;
    ensure!(minute < 60, "invalid timezone minutes");
    let seconds = (hour * 60 + minute) * 60;
    Ok(time::UtcOffset::from_whole_seconds(
        if zone.starts_with('-') {
            -seconds
        } else {
            seconds
        },
    )?)
}
pub(crate) fn history(
    repo: &Repo,
    store: &mut Store<'_>,
    items: &[String],
    list: bool,
    prefix: &std::path::Path,
) -> Result<()> {
    let mut normalized = Vec::new();
    let mut input = items.iter();
    let mut paths = false;
    while let Some(item) = input.next() {
        if item == "--" {
            paths = true;
        }
        if paths {
            normalized.push(item.clone());
            continue;
        }
        if item == "--author" || item == "--grep" {
            normalized.push(format!(
                "{item}={}",
                input.next().context("missing search pattern")?
            ));
        } else if item == "-n" || item == "--max-count" {
            let count = input.next().context("missing history count")?;
            count.parse::<usize>().context("invalid history count")?;
            normalized.push(format!("--max-count={count}"));
        } else if item.starts_with('-')
            && item.len() > 1
            && item[1..].bytes().all(|b| b.is_ascii_digit())
        {
            normalized.push(format!("--max-count={}", &item[1..]));
        } else {
            normalized.push(item.clone());
        }
    }
    let items = &normalized;
    let separator = items.iter().position(|a| a == "--").unwrap_or(items.len());
    let pos = args(
        &items[..separator],
        &[
            "--count",
            "--first-parent",
            "--oneline",
            "--color=",
            "--no-color",
            "-1",
            "--max-count=",
            "--format=",
            "--pretty=",
            "--date=",
            "--all",
            "--graph",
            "--decorate",
            "--decorate=",
            "--no-decorate",
            "--reverse",
            "--author=",
            "--grep=",
            "-i",
            "--regexp-ignore-case",
            "-F",
            "--fixed-strings",
            "-E",
            "--extended-regexp",
            "--basic-regexp",
            "--all-match",
        ],
    )?;
    let mut ids = crate::inspect::history_ids(
        repo,
        store,
        &pos,
        flag(items, "--all"),
        flag(items, "--first-parent"),
    )?;
    if separator < items.len() {
        let specs = crate::workflow::paths(&items[separator + 1..], prefix)?;
        let mut filtered = Vec::new();
        for id in ids {
            if crate::inspect::changed(store, id, &specs)? {
                filtered.push(id);
            }
        }
        ids = filtered;
    }
    ensure!(
        !flag(items, "--reverse") || !flag(items, "--graph"),
        "--graph and --reverse are incompatible"
    );
    if flag(items, "--reverse") {
        ids.reverse();
    }
    let patterns = |prefix: &str| {
        items
            .iter()
            .filter_map(|s| s.strip_prefix(prefix).map(str::to_owned))
            .collect::<Vec<_>>()
    };
    let fixed = flag(items, "-F") || flag(items, "--fixed-strings");
    let extended = flag(items, "-E") || flag(items, "--extended-regexp");
    let insensitive = flag(items, "-i") || flag(items, "--regexp-ignore-case");
    let authors =
        crate::search::Matcher::new(&patterns("--author="), fixed, extended, insensitive)?;
    let messages = crate::search::Matcher::new(&patterns("--grep="), fixed, extended, insensitive)?;
    let mut filtered = Vec::new();
    for id in ids {
        let object = store.get(id)?;
        let author = porcelain::author(&object.data)?;
        let message = porcelain::message(&object.data)?;
        if authors.matches(author.as_bytes())
            && if flag(items, "--all-match") {
                messages.matches_all(message)
            } else {
                messages.matches(message)
            }
        {
            filtered.push(id);
        }
    }
    let ids = filtered;
    let decorate = (flag(items, "--decorate") || option(items, "--decorate=").is_some())
        && !flag(items, "--no-decorate");
    let decorations = if decorate {
        crate::inspect::decorations(repo)?
    } else {
        HashMap::new()
    };
    let mut graph = crate::inspect::Graph::default();
    let dated: Vec<_> = ids.into_iter().map(|id| (id, 0i64)).collect();
    let mut dated = dated;
    let max = if flag(items, "-1") {
        1
    } else {
        option(items, "--max-count=")
            .map(str::parse)
            .transpose()?
            .unwrap_or(usize::MAX)
    };
    dated.truncate(max);
    if flag(items, "--count") {
        println!("{}", dated.len());
        return Ok(());
    }
    let mut first = true;
    for (id, _) in dated {
        let decoration = decorations
            .get(&id)
            .map(|v| format!(" ({})", v.join(", ")))
            .unwrap_or_default();
        let (graph_prefix, transition) = if flag(items, "--graph") {
            graph.prefix(id, &store.parents(id)?)?
        } else {
            (String::new(), None)
        };

        if list {
            println!("{id}");
            continue;
        }
        let continuation = if flag(items, "--graph") {
            graph.continuation()
        } else {
            String::new()
        };
        let object = store.get(id)?;
        let subject = porcelain::subject(&object.data);
        let format = option(items, "--format=").or(option(items, "--pretty="));
        if flag(items, "--oneline") || format == Some("oneline") {
            println!(
                "{graph_prefix}{}{decoration} {subject}",
                crate::output::short(id)
            );
            if let Some(transition) = transition {
                println!("{transition}");
            }
            continue;
        }
        let author = object
            .data
            .split(|b| *b == b'\n')
            .find_map(|l| l.strip_prefix(b"author "))
            .context("missing author")?;
        let author = String::from_utf8_lossy(author);
        let mut fields = author.rsplitn(3, ' ');
        let zone = fields.next().context("invalid author")?;
        let seconds = fields.next().context("invalid author")?.parse::<i64>()?;
        let who = fields.next().context("invalid author")?;
        if let Some(format) = format {
            let commit_time = timestamp(&object.data);
            let zone = object
                .data
                .split(|b| *b == b'\n')
                .find_map(|l| l.strip_prefix(b"committer "))
                .and_then(|l| std::str::from_utf8(l).ok())
                .and_then(|l| l.rsplit(' ').next())
                .context("invalid committer timezone")?;
            let date =
                time::OffsetDateTime::from_unix_timestamp(commit_time)?.to_offset(timezone(zone)?);
            let date = match option(items, "--date=") {
                Some("format:%Y%m%d") => format!(
                    "{:04}{:02}{:02}",
                    date.year(),
                    u8::from(date.month()),
                    date.day()
                ),
                Some("short") => format!(
                    "{:04}-{:02}-{:02}",
                    date.year(),
                    u8::from(date.month()),
                    date.day()
                ),
                Some(_) => anyhow::bail!("unsupported log date format"),
                None => format!(
                    "{} {} {} {:02}:{:02}:{:02} {} {}",
                    ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
                        [date.weekday().number_days_from_monday() as usize],
                    [
                        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct",
                        "Nov", "Dec"
                    ][u8::from(date.month()) as usize - 1],
                    date.day(),
                    date.hour(),
                    date.minute(),
                    date.second(),
                    date.year(),
                    zone
                ),
            };
            let values = [
                ("%H", id.to_string()),
                ("%h", crate::output::short(id)),
                ("%s", subject),
                ("%ct", commit_time.to_string()),
                ("%at", seconds.to_string()),
                ("%cd", date),
                ("%n", "\n".into()),
                ("%%", "%".into()),
            ];
            let mut rest = format;
            let mut rendered = String::new();
            while let Some(index) = rest.find('%') {
                rendered.push_str(&rest[..index]);
                rest = &rest[index..];
                let (key, value) = values
                    .iter()
                    .find(|(key, _)| rest.starts_with(key))
                    .context("unsupported log format")?;
                rendered.push_str(value);
                rest = &rest[key.len()..];
            }
            rendered.push_str(rest);
            for (n, line) in text(&rendered).lines().enumerate() {
                println!(
                    "{}{line}",
                    if n == 0 { &graph_prefix } else { &continuation }
                );
            }
            if let Some(transition) = transition {
                println!("{transition}");
            }
            continue;
        }
        let date = time::OffsetDateTime::from_unix_timestamp(seconds)?;
        let date = date.to_offset(timezone(zone)?);
        let days = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
        let months = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        if !first {
            println!("{continuation}");
        }
        first = false;
        println!("{graph_prefix}commit {id}{decoration}");
        if let Some(transition) = transition {
            println!("{transition}");
        }
        let parents = store.parents(id)?;
        if parents.len() > 1 {
            println!(
                "{continuation}Merge: {}",
                parents
                    .into_iter()
                    .map(crate::output::short)
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        println!(
            "{continuation}Author: {}\n{continuation}Date:   {} {} {} {:02}:{:02}:{:02} {} {}\n{continuation}",
            text(who),
            days[date.weekday().number_days_from_monday() as usize],
            months[u8::from(date.month()) as usize - 1],
            date.day(),
            date.hour(),
            date.minute(),
            date.second(),
            date.year(),
            zone
        );
        let end = object
            .data
            .windows(2)
            .position(|w| w == b"\n\n")
            .context("invalid commit")?;
        for line in String::from_utf8_lossy(&object.data[end + 2..])
            .trim_end_matches('\n')
            .lines()
        {
            println!("{continuation}    {}", text(line));
        }
    }
    Ok(())
}
fn describe(repo: &Repo, store: &mut Store<'_>, items: &[String]) -> Result<()> {
    let mut filtered = Vec::new();
    let mut patterns = Vec::new();
    let mut i = 0;
    while i < items.len() {
        if items[i] == "--match" {
            i += 1;
            patterns.push(items.get(i).context("--match needs a pattern")?.as_str());
        } else if let Some(p) = items[i].strip_prefix("--match=") {
            patterns.push(p);
        } else {
            filtered.push(items[i].clone());
        }
        i += 1;
    }
    let pos = args(
        &filtered,
        &[
            "--tags",
            "--long",
            "--always",
            "--dirty",
            "--dirty=",
            "--abbrev=",
            "--first-parent",
            "--exact-match",
        ],
    )?;
    let id = revision::commit(repo, store, one(&pos, "HEAD")?)?;
    let history = revision::history(store, id, None, flag(items, "--first-parent"))?;
    let reachable: HashSet<_> = history.iter().copied().collect();
    let mut candidates = Vec::new();
    let mut graph_budget = crate::object::MAX_OBJECTS * 8;
    let mut distances = HashMap::new();
    for (name, tag) in repo.refs()? {
        let Some(name) = name.strip_prefix("refs/tags/") else {
            continue;
        };
        if !patterns.is_empty() && !patterns.iter().any(|p| glob(p, name)) {
            continue;
        }
        let annotated = store.get(tag)?.kind == Kind::Tag;
        if !flag(items, "--tags") && !annotated {
            continue;
        }
        let target = revision::peel(store, tag)?;
        if !reachable.contains(&target) {
            continue;
        }
        let distance = if let Some(distance) = distances.get(&target) {
            *distance
        } else {
            ensure!(
                graph_budget >= reachable.len(),
                "describe graph budget exceeded"
            );
            graph_budget -= reachable.len();
            let ancestors = crate::object::commits(store, &[target])?;
            let distance = reachable.difference(&ancestors).count();
            distances.insert(target, distance);
            distance
        };
        candidates.push((distance, !annotated, name.to_owned()));
    }
    candidates.sort();
    let length = option(items, "--abbrev=").unwrap_or("7").parse::<usize>()?;
    ensure!(length <= 40, "invalid abbreviation length");
    let mut value = if let Some((distance, _, name)) = candidates.first() {
        ensure!(
            !flag(items, "--exact-match") || *distance == 0,
            "no tag exactly matches"
        );
        if length == 0 || *distance == 0 && !flag(items, "--long") {
            name.clone()
        } else {
            format!("{name}-{distance}-g{}", abbrev(store, id, length.max(4))?)
        }
    } else {
        ensure!(
            flag(items, "--always") && !flag(items, "--exact-match"),
            "No names found, cannot describe anything."
        );
        abbrev(store, id, length.max(4))?
    };
    if flag(items, "--dirty") || option(items, "--dirty=").is_some() {
        let staged = porcelain::index(repo)?;
        if porcelain::tree(store, id)? != staged
            || crate::checkout::snapshot(repo, &staged)? != staged
        {
            value.push_str(option(items, "--dirty=").unwrap_or("-dirty"));
        }
    }
    println!("{}", text(&value));
    Ok(())
}
