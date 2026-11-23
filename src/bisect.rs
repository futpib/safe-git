//! A data-only bisect state machine; test commands and shell fragments are never run.
use crate::{
    Cli,
    object::{MAX_OBJECTS, Oid, ancestor, commits},
    porcelain,
    repo::{Repo, Store},
    revision,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
const STATE: &str = "safe-git-bisect";
#[derive(Serialize, Deserialize)]
struct State {
    original: String,
    tip: Oid,
    bad: Option<Oid>,
    good: BTreeSet<Oid>,
    skip: BTreeSet<Oid>,
    log: Vec<String>,
}
fn save(repo: &Repo, state: &State) -> Result<()> {
    let data = serde_json::to_vec(state)?;
    ensure!(data.len() <= 8 * 1024 * 1024, "bisect state exceeds limit");
    repo.atomic(STATE, &data)
}
fn load(repo: &Repo) -> Result<State> {
    let s: State = serde_json::from_slice(
        &repo
            .read(STATE, 8 * 1024 * 1024)?
            .context("bisect has not started")?,
    )?;
    crate::repo::reference(&s.original)?;
    ensure!(
        s.good.len() + s.skip.len() <= MAX_OBJECTS && s.log.len() <= MAX_OBJECTS,
        "bisect state exceeds limit"
    );
    Ok(s)
}
pub fn run(cli: &Cli, repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    ensure!(
        !repo.exists("safe-git-sequence")?,
        "finish the current operation first"
    );
    let command = args.first().map(String::as_str).unwrap_or("log");
    if command == "start" {
        ensure!(
            !repo.exists(STATE)?,
            "bisect already in progress; reset it first"
        );
        ensure!(args.len() <= 3, "bisect start accepts BAD GOOD");
        let tip = repo.read_ref("HEAD")?.context("unborn HEAD")?;
        let index = porcelain::index(repo)?;
        ensure!(
            index == porcelain::tree(store, tip)?
                && crate::checkout::snapshot(repo, &index)? == index,
            "bisect requires a clean working tree"
        );
        let mut state = State {
            original: repo.head()?,
            tip,
            bad: None,
            good: BTreeSet::new(),
            skip: BTreeSet::new(),
            log: vec!["git bisect start".into()],
        };
        if let Some(bad) = args.get(1) {
            let id = revision::commit(repo, store, bad)?;
            state.bad = Some(id);
            state.log.push(format!("git bisect bad {id}"));
        }
        if let Some(good) = args.get(2) {
            let id = revision::commit(repo, store, good)?;
            state.good.insert(id);
            state.log.push(format!("git bisect good {id}"));
        }
        save(repo, &state)?;
        return next(cli, repo, store, &state);
    }
    let mut state = load(repo)?;
    match command {
        "good" | "bad" | "skip" => {
            ensure!(
                command != "bad" || args.len() <= 2,
                "bad accepts one commit"
            );
            let values = if args.len() == 1 {
                vec!["HEAD".into()]
            } else {
                args[1..].to_vec()
            };
            for value in values {
                let id = revision::commit(repo, store, &value)?;
                match command {
                    "good" => {
                        state.good.insert(id);
                    }
                    "skip" => {
                        state.skip.insert(id);
                    }
                    _ => state.bad = Some(id),
                }
                state.log.push(format!("git bisect {command} {id}"));
            }
            save(repo, &state)?;
            next(cli, repo, store, &state)
        }
        "log" => {
            ensure!(args.len() <= 1, "log takes no arguments");
            for line in state.log {
                println!("{}", crate::compat::text(&line));
            }
            Ok(())
        }
        "reset" => {
            ensure!(args.len() <= 2, "reset accepts one revision");
            let target = if let Some(target) = args.get(1) {
                target.clone()
            } else if state.original == "HEAD" {
                state.tip.to_string()
            } else {
                ensure!(
                    repo.read_ref(&state.original)? == Some(state.tip),
                    "original branch changed during bisect"
                );
                state.original.clone()
            };
            let branch = target.strip_prefix("refs/heads/");
            porcelain::checkout(repo, store, &target, branch, false, cli.quiet)?;
            repo.remove(STATE)
        }
        _ => bail!("unsupported bisect command {command}; executable test commands are never run"),
    }
}
fn next(cli: &Cli, repo: &Repo, store: &mut Store<'_>, state: &State) -> Result<()> {
    let Some(bad) = state.bad else {
        println!("Mark a bad commit with bisect bad");
        return Ok(());
    };
    if state.good.is_empty() {
        println!("Mark a good commit with bisect good");
        return Ok(());
    }
    for good in &state.good {
        ensure!(
            *good != bad && ancestor(store, *good, bad)?,
            "good commits must be ancestors of the bad commit"
        );
    }
    let excluded = commits(store, &state.good.iter().copied().collect::<Vec<_>>())?;
    let candidates: BTreeSet<_> = commits(store, &[bad])?
        .difference(&excluded)
        .copied()
        .collect();
    if candidates.len() == 1 {
        println!("{bad} is the first bad commit");
        return Ok(());
    }
    let mut best = None;
    let mut budget = 4_000_000usize;
    for candidate in &candidates {
        if *candidate == bad || state.skip.contains(candidate) {
            continue;
        }
        let ancestors = commits(store, &[*candidate])?;
        budget = budget
            .checked_sub(ancestors.len())
            .context("bisect comparison budget exceeded")?;
        let count = candidates.iter().filter(|c| ancestors.contains(c)).count();
        let score = count.min(candidates.len() - count);
        if best.is_none_or(|(_, s)| score > s) {
            best = Some((*candidate, score));
        }
    }
    if let Some((id, _)) = best {
        porcelain::checkout(repo, store, &id.to_string(), None, false, cli.quiet)?;
        println!("Bisecting {} remaining commits: {id}", candidates.len());
    } else {
        println!("Skipped commits prevent identifying a unique first bad commit:");
        for id in candidates {
            println!("{id}");
        }
    }
    Ok(())
}
