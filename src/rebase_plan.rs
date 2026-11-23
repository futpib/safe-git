//! Rebase plans contain only typed history edits. Labels are in-memory names, never paths.
use crate::{
    object::{Oid, ReadObjects},
    repo::Store,
    sequence::Step,
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, Read, Write},
};
pub fn label(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
        "invalid rebase label"
    );
    Ok(())
}
fn command(action: &str, target: String) -> Step {
    Step {
        id: Oid::ZERO,
        action: action.into(),
        message: None,
        targets: vec![target],
    }
}
pub fn graph(
    store: &mut Store<'_>,
    steps: Vec<Step>,
    fork: Oid,
    cousins: bool,
    labels: &mut BTreeMap<String, Oid>,
) -> Result<Vec<Step>> {
    let mut out = Vec::new();
    let mut visited = BTreeSet::new();
    let mut tip = None;
    for mut step in steps {
        let parents = store.parents(step.id)?;
        ensure!(parents.len() <= 2, "octopus rebase merges are unsupported");
        let mut targets = Vec::new();
        for parent in parents {
            let target = if visited.contains(&parent) {
                format!("commit-{parent}")
            } else if parent == fork || cousins {
                "onto".into()
            } else {
                let name = format!("base-{parent}");
                labels.insert(name.clone(), parent);
                name
            };
            targets.push(target);
        }
        let reset = if matches!(step.action.as_str(), "fixup" | "squash") {
            format!("commit-{}", tip.context("fixup has no prior commit")?)
        } else {
            targets.first().cloned().unwrap_or("onto".into())
        };
        out.push(command("reset", reset));
        if step.action == "merge" {
            step.targets = targets.into_iter().skip(1).collect();
        }
        let id = step.id;
        out.push(step);
        out.push(command("label", format!("commit-{id}")));
        visited.insert(id);
        tip = Some(id);
    }
    if let Some(tip) = tip {
        out.push(command("reset", format!("commit-{tip}")));
    }
    Ok(out)
}
pub fn interactive(steps: Vec<Step>, initial: &BTreeMap<String, Oid>) -> Result<Vec<Step>> {
    for step in &steps {
        match step.action.as_str() {
            "label" | "reset" => println!("{} {}", step.action, step.targets.join(" ")),
            "break" => println!("break"),
            "merge" => println!("merge -C {} {}", step.id, step.targets.join(" ")),
            _ => println!("{} {}", step.action, step.id),
        }
    }
    println!(
        "Enter pick/reword/edit/squash/fixup/drop COMMIT [message], label NAME, reset NAME, or merge -C COMMIT LABEL lines, then done. Enter done alone to keep this plan. Editors and exec are never run."
    );
    std::io::stdout().flush()?;
    let mut chosen = Vec::new();
    let mut seen = BTreeSet::new();
    let mut known: BTreeMap<_, _> = initial.keys().map(|k| (k.clone(), false)).collect();
    let mut has_commit = false;
    let mut input = std::io::stdin().lock();
    let mut bytes = 0;
    loop {
        let mut line = String::new();
        let n = (&mut input).take(8193).read_line(&mut line)?;
        bytes += n;
        ensure!(
            n > 0 && n <= 8192 && bytes <= 8 * 1024 * 1024,
            "missing or oversized rebase plan"
        );
        let line = line.trim();
        if line == "done" {
            break;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "break" || line == "b" {
            chosen.push(Step {
                id: Oid::ZERO,
                action: "break".into(),
                message: None,
                targets: Vec::new(),
            });
            continue;
        }
        let (action, rest) = line
            .split_once(char::is_whitespace)
            .context("rebase action needs an argument")?;
        let rest = rest.trim_start();
        let action = match action {
            "p" | "pick" => "pick",
            "r" | "reword" => "reword",
            "e" | "edit" => "edit",
            "s" | "squash" => "squash",
            "f" | "fixup" => "fixup",
            "d" | "drop" => "drop",
            "l" | "label" => "label",
            "t" | "reset" => "reset",
            "m" | "merge" => "merge",
            _ => bail!("unsupported rebase action; exec and scripts are never run"),
        };
        if action == "label" || action == "reset" {
            let target = rest.split_whitespace().next().context("missing label")?;
            label(target)?;
            if action == "label" {
                ensure!(
                    known.insert(target.into(), has_commit).is_none(),
                    "duplicate rebase label"
                );
            } else {
                has_commit = *known.get(target).context("unknown rebase label")?;
            }
            chosen.push(command(action, target.into()));
            continue;
        }
        let mut fields = rest.splitn(2, char::is_whitespace);
        let first = fields.next().unwrap();
        let mut rest = fields.next().unwrap_or("").trim_start();
        let id = if action == "merge" {
            ensure!(
                first == "-C" || first == "-c",
                "merge requires -C or -c and a commit"
            );
            let mut fields = rest.splitn(2, char::is_whitespace);
            let id = fields.next().context("missing merge commit")?;
            rest = fields.next().context("missing merge label")?.trim_start();
            id
        } else {
            first
        };
        let matches: Vec<_> = steps
            .iter()
            .filter(|s| !s.id.is_zero() && id.len() >= 4 && s.id.to_string().starts_with(id))
            .collect();
        ensure!(matches.len() == 1, "unknown or ambiguous plan commit");
        let mut step = matches[0].clone();
        ensure!(seen.insert(step.id), "duplicate plan commit");
        step.action = action.into();
        step.message = None;
        step.targets.clear();
        if action == "merge" {
            let (labels, message) = rest.split_once(" # ").unwrap_or((rest, ""));
            for target in labels.split_whitespace() {
                label(target)?;
                ensure!(known.contains_key(target), "unknown merge label");
                step.targets.push(target.into());
            }
            ensure!(step.targets.len() == 1, "merge requires one label");
            if first == "-c" {
                ensure!(!message.is_empty(), "merge -c requires # MESSAGE");
                step.message = Some(message.into());
            }
        } else if action == "reword" {
            ensure!(
                !rest.is_empty(),
                "reword requires a message on the plan line"
            );
            step.message = Some(rest.into());
        }
        if matches!(action, "squash" | "fixup") {
            ensure!(has_commit, "cannot squash the first commit after a reset");
        }
        if action != "drop" {
            has_commit = true;
        }
        chosen.push(step);
        ensure!(
            chosen.len() <= crate::object::MAX_OBJECTS * 4,
            "rebase plan exceeds limit"
        );
    }
    Ok(if chosen.is_empty() { steps } else { chosen })
}
