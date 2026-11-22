//! Resumable history operations. The sequence contains typed data only; no exec/editor hooks.
use crate::{
    Cli,
    checkout::{self, Edit},
    index::{Conflicts, Entries},
    object::{Kind, Object, Oid, ReadObjects, ancestor},
    porcelain,
    repo::{Repo, Store},
    revision,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    io::{BufRead, Read, Write},
};
const STATE: &str = "safe-git-sequence";
#[derive(Clone, Serialize, Deserialize)]
struct Step {
    id: Oid,
    action: String,
    message: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct State {
    kind: String,
    branch: String,
    original: Oid,
    current: Oid,
    steps: Vec<Step>,
    next: usize,
    pending: bool,
    #[serde(default)]
    installed: bool,
    editing: bool,
    mainline: Option<usize>,
    no_commit: bool,
    merge_parent: Option<Oid>,
}
fn save(repo: &Repo, state: &State) -> Result<()> {
    let bytes = serde_json::to_vec(state)?;
    ensure!(bytes.len() <= 8 * 1024 * 1024, "sequence exceeds limit");
    repo.atomic(STATE, &bytes)
}
fn load(repo: &Repo) -> Result<State> {
    let bytes = repo
        .read(STATE, 8 * 1024 * 1024)?
        .context("no operation in progress")?;
    let state: State = serde_json::from_slice(&bytes)?;
    ensure!(
        matches!(
            state.kind.as_str(),
            "merge" | "cherry-pick" | "revert" | "rebase"
        ),
        "invalid sequence kind"
    );
    crate::repo::reference(&state.branch)?;
    ensure!(
        state.next <= state.steps.len() && state.steps.len() <= crate::object::MAX_OBJECTS,
        "invalid sequence position"
    );
    for step in &state.steps {
        ensure!(
            matches!(
                step.action.as_str(),
                "pick" | "reword" | "edit" | "squash" | "fixup" | "drop"
            ),
            "unsupported sequence action; commands are never executed"
        );
    }
    ensure!(repo.head()? == state.branch, "sequence branch changed");
    Ok(state)
}
fn clean(repo: &Repo, store: &mut Store<'_>, tip: Oid) -> Result<()> {
    ensure!(
        porcelain::index(repo)? == porcelain::tree(store, tip)?,
        "staged changes would be overwritten"
    );
    let index = porcelain::index(repo)?;
    ensure!(
        checkout::snapshot(repo, &index)? == index,
        "local changes would be overwritten"
    );
    Ok(())
}
fn merge_trees(
    repo: &Repo,
    store: &mut Store<'_>,
    base: &Entries,
    ours: &Entries,
    theirs: &Entries,
) -> Result<(Entries, Entries, Conflicts)> {
    let mut work = Entries::new();
    let mut index = Entries::new();
    let mut conflicts = Conflicts::new();
    let paths: BTreeSet<_> = base
        .keys()
        .chain(ours.keys())
        .chain(theirs.keys())
        .collect();
    let mut total = 0;
    for path in paths {
        let a = base.get(path).copied();
        let b = ours.get(path).copied();
        let c = theirs.get(path).copied();
        let one = |entry: Option<crate::index::Entry>| {
            entry
                .map(|e| (path.clone(), e))
                .into_iter()
                .collect::<Entries>()
        };
        match porcelain::three_way(repo, store, &one(a), &one(b), &one(c)) {
            Ok(merged) => {
                work.extend(merged.clone());
                index.extend(merged);
                continue;
            }
            Err(error) => ensure!(
                error.to_string().starts_with("merge conflict")
                    || error.to_string().starts_with("binary merge conflict"),
                "{error:#}"
            ),
        }
        let mut actual = b.or(c);
        if a.into_iter()
            .chain(b)
            .chain(c)
            .all(|e| matches!(e.mode, 0o100644 | 0o100755))
        {
            let mut values = Vec::new();
            for entry in [a, b, c] {
                let bytes = if let Some(e) = entry {
                    let o = store.get(e.oid)?;
                    ensure!(o.kind == Kind::Blob, "expected merge blob");
                    o.data.clone()
                } else {
                    Vec::new()
                };
                total += bytes.len();
                ensure!(
                    total <= crate::object::MAX_TOTAL,
                    "merge exceeds byte limit"
                );
                values.push(bytes);
            }
            if !values.iter().any(|v| v.contains(&0)) {
                crate::compat::diff_budget(&values[0], &values[1])?;
                crate::compat::diff_budget(&values[0], &values[2])?;
                let data =
                    diffy::merge_bytes(&values[0], &values[1], &values[2]).unwrap_or_else(|v| v);
                let object = Object {
                    kind: Kind::Blob,
                    data,
                };
                let oid = object.oid()?;
                repo.write_object(&object)?;
                if let Some(entry) = &mut actual {
                    entry.oid = oid;
                }
            }
        }
        if let Some(entry) = actual {
            work.insert(path.clone(), entry);
        }
        conflicts.insert(path.clone(), [a, b, c]);
    }
    crate::index::no_prefix_conflicts(&work)?;
    Ok((work, index, conflicts))
}
fn install(
    repo: &Repo,
    store: &mut Store<'_>,
    state: &State,
    after: Entries,
    index_after: Entries,
    conflicts_after: Conflicts,
) -> Result<()> {
    let (index_before, conflicts_before) = porcelain::index_full(repo)?;
    let mut tracked = index_before.clone();
    for (p, stages) in &conflicts_before {
        if let Some(e) = stages.iter().flatten().next() {
            tracked.insert(p.clone(), *e);
        }
    }
    let before = checkout::snapshot(repo, &tracked)?;
    checkout::edit(
        repo,
        store,
        Edit {
            head: state.branch.clone(),
            tip: state.current,
            before,
            after,
            index_before,
            index_after,
            conflicts_before,
            conflicts_after,
        },
    )
}
fn create(
    repo: &Repo,
    store: &mut Store<'_>,
    cli: &Cli,
    state: &mut State,
    step: &Step,
) -> Result<()> {
    let index = porcelain::index(repo)?;
    let object = store.get(step.id)?;
    let mut message = match &step.message {
        Some(v) => v.as_bytes().to_vec(),
        None => {
            if state.kind == "revert" {
                format!(
                    "Revert \"{}\"\n\nThis reverts commit {}.\n",
                    porcelain::subject(&object.data),
                    step.id
                )
                .into_bytes()
            } else if state.kind == "merge" {
                format!("Merge commit '{}'\n", step.id).into_bytes()
            } else {
                porcelain::message(&object.data)?.to_vec()
            }
        }
    };
    let squash = matches!(step.action.as_str(), "squash" | "fixup");
    let previous = store.get(state.current)?;
    let mut author = if state.kind == "revert" || state.kind == "merge" {
        porcelain::identity(repo, &cli.config, "AUTHOR")?
    } else {
        porcelain::author(&object.data)?
    };
    let parents = if squash {
        ensure!(state.next > 0, "cannot squash the first rebase step");
        author = porcelain::author(&previous.data)?;
        let mut prior = porcelain::message(&previous.data)?.to_vec();
        if step.action == "squash" {
            prior.extend_from_slice(b"\n");
            prior.extend_from_slice(&message);
        }
        message = prior;
        previous.commit()?.1
    } else if let Some(other) = state.merge_parent {
        vec![state.current, other]
    } else {
        vec![state.current]
    };
    let tree = porcelain::write_tree(repo, &index)?;
    let id = porcelain::new_commit(
        repo,
        tree,
        &parents,
        &author,
        &porcelain::identity(repo, &cli.config, "COMMITTER")?,
        &message,
    )?;
    let logs = crate::history::logs(
        repo,
        &state.branch,
        Some(state.current),
        id,
        &format!(
            "{}: {}",
            state.kind,
            porcelain::subject(&store.get(id)?.data)
        ),
    )?;
    repo.update_ref(&state.branch, Some(state.current), id)?;
    crate::history::finish_logs(logs)?;
    state.current = id;
    state.pending = false;
    state.next += 1;
    state.editing = step.action == "edit";
    save(repo, state)
}
fn advance(cli: &Cli, repo: &Repo, store: &mut Store<'_>, state: &mut State) -> Result<()> {
    while state.next < state.steps.len() {
        let step = state.steps[state.next].clone();
        if step.action == "drop" {
            state.next += 1;
            save(repo, state)?;
            continue;
        }
        let object = store.get(step.id)?;
        let parents = object.commit()?.1;
        let (base, theirs) = if state.kind == "merge" {
            {
                let base = revision::merge_base(store, state.current, step.id)?;
                (
                    porcelain::tree(store, base)?,
                    porcelain::tree(store, step.id)?,
                )
            }
        } else {
            let parent = if parents.len() > 1 {
                parents
                    .get(
                        state
                            .mainline
                            .context("merge commit requires -m MAINLINE")?
                            .checked_sub(1)
                            .context("mainline starts at 1")?,
                    )
                    .copied()
                    .context("invalid mainline")?
            } else {
                parents.first().copied().unwrap_or(Oid::ZERO)
            };
            let prior = if parent == Oid::ZERO {
                Entries::new()
            } else {
                porcelain::tree(store, parent)?
            };
            let after = porcelain::tree(store, step.id)?;
            if state.kind == "revert" {
                (after, prior)
            } else {
                (prior, after)
            }
        };
        let ours = if state.no_commit {
            porcelain::index(repo)?
        } else {
            porcelain::tree(store, state.current)?
        };
        let (after, index, conflicts) = merge_trees(repo, store, &base, &ours, &theirs)?;
        state.pending = true;
        state.installed = false;
        save(repo, state)?;
        let unresolved = !conflicts.is_empty();
        install(repo, store, state, after, index, conflicts)?;
        state.installed = true;
        save(repo, state)?;
        if unresolved {
            bail!(
                "{} has conflicts; resolve files, add them, then run {} --continue (or --abort)",
                state.kind,
                state.kind
            );
        }
        if state.no_commit {
            state.next += 1;
            state.pending = false;
            save(repo, state)?;
        } else {
            create(repo, store, cli, state, &step)?;
        }
        if state.editing {
            println!("Stopped for editing. Amend the commit, then run rebase --continue.");
            return Ok(());
        }
    }
    repo.remove(STATE)?;
    if !cli.quiet {
        println!("{} completed", state.kind);
    }
    Ok(())
}
fn control(cli: &Cli, repo: &Repo, store: &mut Store<'_>, kind: &str, action: &str) -> Result<()> {
    let mut state = load(repo)?;
    ensure!(state.kind == kind, "different operation in progress");
    if action == "--abort" {
        // Reset uses the existing checkout journal and backs up every replaced file.
        porcelain::reset(repo, store, state.original, cli.quiet)?;
        repo.remove(STATE)?;
        return Ok(());
    }
    if state.editing {
        state.current = repo.read_ref(&state.branch)?.context("missing branch")?;
        clean(repo, store, state.current)?;
        state.editing = false;
        save(repo, &state)?;
    } else {
        ensure!(
            repo.read_ref(&state.branch)? == Some(state.current),
            "HEAD changed during operation"
        );
        ensure!(state.pending, "no step waiting to continue");
        ensure!(
            state.installed || action == "--skip",
            "sequence was interrupted before installation completed; abort or skip this step"
        );
        if action == "--skip" {
            porcelain::reset(repo, store, state.current, true)?;
            state.next += 1;
            state.pending = false;
            save(repo, &state)?;
        } else {
            let step = state
                .steps
                .get(state.next)
                .context("invalid pending step")?
                .clone();
            let index = porcelain::index(repo)?;
            ensure!(
                checkout::snapshot(repo, &index)? == index,
                "unstaged changes remain; add resolved files first"
            );
            if state.no_commit {
                state.next += 1;
                state.pending = false;
                save(repo, &state)?;
            } else {
                create(repo, store, cli, &mut state, &step)?;
            }
        }
    }
    advance(cli, repo, store, &mut state)
}
fn interactive(steps: Vec<Step>) -> Result<Vec<Step>> {
    for step in &steps {
        println!("pick {}", step.id);
    }
    println!(
        "Enter pick/reword/edit/squash/fixup/drop COMMIT [message] lines, then done. Enter done alone to keep this order. Commands and editors are never run."
    );
    std::io::stdout().flush()?;
    let mut chosen = Vec::new();
    let mut seen = BTreeSet::new();
    let mut input = std::io::stdin().lock();
    loop {
        let mut line = String::new();
        let n = (&mut input).take(8193).read_line(&mut line)?;
        ensure!(n > 0 && n <= 8192, "missing or oversized rebase plan line");
        let line = line.trim();
        if line == "done" {
            break;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.splitn(3, ' ');
        let action = words.next().unwrap();
        let id = words.next().context("rebase action needs a commit")?;
        let action = match action {
            "p" | "pick" => "pick",
            "r" | "reword" => "reword",
            "e" | "edit" => "edit",
            "s" | "squash" => "squash",
            "f" | "fixup" => "fixup",
            "d" | "drop" => "drop",
            _ => bail!("unsupported rebase action; exec and scripts are never run"),
        };
        let matches: Vec<_> = steps
            .iter()
            .filter(|s| id.len() >= 4 && s.id.to_string().starts_with(id))
            .collect();
        ensure!(matches.len() == 1, "unknown or ambiguous plan commit");
        let mut step = matches[0].clone();
        ensure!(seen.insert(step.id.to_string()), "duplicate plan commit");
        step.action = action.into();
        step.message = words.next().map(str::to_owned);
        ensure!(
            action != "reword" || step.message.as_ref().is_some_and(|m| !m.trim().is_empty()),
            "reword requires a message on the plan line"
        );
        ensure!(
            !matches!(action, "squash" | "fixup")
                || chosen.iter().any(|s: &Step| s.action != "drop"),
            "cannot squash the first commit"
        );
        chosen.push(step);
        ensure!(chosen.len() <= steps.len(), "plan exceeds history");
    }
    Ok(if chosen.is_empty() { steps } else { chosen })
}
pub fn run(
    cli: &Cli,
    repo: &Repo,
    store: &mut Store<'_>,
    kind: &str,
    args: &[String],
) -> Result<()> {
    if args.len() == 1 && matches!(args[0].as_str(), "--continue" | "--abort" | "--skip") {
        return control(cli, repo, store, kind, &args[0]);
    }
    ensure!(
        !repo.exists(STATE)?,
        "operation in progress; continue or abort it first"
    );
    let mut no_commit = false;
    let mut mainline = None;
    let mut interactive_rebase = false;
    let mut values = Vec::new();
    let mut items = args.iter();
    while let Some(arg) = items.next() {
        match arg.as_str() {
            "-n" | "--no-commit" => no_commit = true,
            "-i" | "--interactive" if kind == "rebase" => interactive_rebase = true,
            "-m" | "--mainline" => {
                mainline = Some(items.next().context("missing mainline")?.parse::<usize>()?)
            }
            "--stat" | "-q" | "--quiet" => {}
            a if a.starts_with('-') => bail!("unsupported {kind} option {a}"),
            _ => values.push(arg.as_str()),
        }
    }
    let branch = repo.head()?;
    let original = repo.read_ref(&branch)?.context("unborn HEAD")?;
    clean(repo, store, original)?;
    let mut state = State {
        kind: kind.into(),
        branch,
        original,
        current: original,
        steps: Vec::new(),
        next: 0,
        pending: false,
        installed: false,
        editing: false,
        mainline,
        no_commit,
        merge_parent: None,
    };
    if kind == "rebase" {
        ensure!(values.len() <= 1, "rebase accepts one upstream");
        let upstream = revision::upstream(repo)?;
        let incoming = revision::commit(repo, store, values.first().copied().unwrap_or(&upstream))?;
        let base = revision::merge_base(store, original, incoming)?;
        let excluded = crate::object::commits(store, &[incoming])?;
        let mut visited = BTreeSet::new();
        let mut pending = vec![(original, false)];
        while let Some((id, expanded)) = pending.pop() {
            if excluded.contains(&id) {
                continue;
            }
            let parents = store.get(id)?.commit()?.1;
            if expanded {
                if parents.len() <= 1 {
                    state.steps.push(Step {
                        id,
                        action: "pick".into(),
                        message: None,
                    });
                }
                continue;
            }
            if !visited.insert(id.to_string()) {
                continue;
            }
            ensure!(
                visited.len() <= crate::object::MAX_OBJECTS
                    && pending.len() <= crate::object::MAX_OBJECTS * 4,
                "rebase exceeds limit"
            );
            pending.push((id, true));
            pending.extend(parents.into_iter().rev().map(|id| (id, false)));
        }
        if interactive_rebase {
            state.steps = interactive(state.steps)?;
        }
        if incoming == base && !interactive_rebase {
            return Ok(());
        }
        save(repo, &state)?;
        porcelain::reset(repo, store, incoming, true)?;
        state.current = incoming;
        save(repo, &state)?;
    } else {
        ensure!(!values.is_empty(), "{kind} requires commits");
        for value in values {
            state.steps.push(Step {
                id: revision::commit(repo, store, value)?,
                action: "pick".into(),
                message: None,
            });
        }
        save(repo, &state)?;
    }
    advance(cli, repo, store, &mut state)
}
pub fn merge(
    cli: &Cli,
    repo: &Repo,
    store: &mut Store<'_>,
    incoming: Oid,
    no_commit: bool,
    no_ff: bool,
) -> Result<()> {
    ensure!(!repo.exists(STATE)?, "operation in progress");
    let branch = repo.head()?;
    let old = repo.read_ref(&branch)?.context("unborn HEAD")?;
    if ancestor(store, incoming, old)? {
        return Ok(());
    }
    if !no_ff && ancestor(store, old, incoming)? {
        return crate::merge(repo, store, incoming, cli.quiet, false);
    }
    clean(repo, store, old)?;
    let mut state = State {
        kind: "merge".into(),
        branch,
        original: old,
        current: old,
        steps: vec![Step {
            id: incoming,
            action: "pick".into(),
            message: None,
        }],
        next: 0,
        pending: false,
        installed: false,
        editing: false,
        mainline: None,
        no_commit,
        merge_parent: Some(incoming),
    };
    save(repo, &state)?;
    // A no-commit merge remains resumable; ordinary cherry-pick -n does not.
    if no_commit {
        let base = revision::merge_base(store, old, incoming)?;
        let a = porcelain::tree(store, base)?;
        let b = porcelain::tree(store, old)?;
        let c = porcelain::tree(store, incoming)?;
        let (work, index, conflicts) = merge_trees(repo, store, &a, &b, &c)?;
        let unresolved = !conflicts.is_empty();
        state.pending = true;
        state.no_commit = false;
        save(repo, &state)?;
        install(repo, store, &state, work, index, conflicts)?;
        state.installed = true;
        save(repo, &state)?;
        if unresolved {
            bail!("merge conflicts; resolve, add, then merge --continue");
        }
        println!("Merge staged; run commit -m MESSAGE or merge --continue.");
        Ok(())
    } else {
        advance(cli, repo, store, &mut state)
    }
}
pub fn commit_pending(
    cli: &Cli,
    repo: &Repo,
    store: &mut Store<'_>,
    message: &str,
) -> Result<bool> {
    if !repo.exists(STATE)? {
        return Ok(false);
    }
    let mut state = load(repo)?;
    if state.editing {
        return Ok(false);
    }
    ensure!(
        state.pending && state.installed,
        "operation is not ready to commit"
    );
    let mut step = state.steps[state.next].clone();
    step.message = Some(message.into());
    create(repo, store, cli, &mut state, &step)?;
    if state.kind == "rebase" {
        state.editing = true;
        save(repo, &state)?;
    } else {
        advance(cli, repo, store, &mut state)?;
    }
    Ok(true)
}

pub fn amend_allowed(repo: &Repo) -> Result<()> {
    if repo.exists(STATE)? {
        ensure!(
            load(repo)?.editing,
            "finish or abort the pending operation before amending"
        );
    }
    Ok(())
}
