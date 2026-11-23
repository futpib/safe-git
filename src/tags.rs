use crate::{
    Cli,
    object::{Kind, Object, ReadObjects},
    porcelain,
    repo::{Repo, Store},
    revision,
};
use anyhow::{Context, Result, bail, ensure};
pub fn is_mutation(args: &[String]) -> bool {
    !args.is_empty()
        && !args
            .iter()
            .any(|a| matches!(a.as_str(), "-l" | "--list") || a.starts_with("--format="))
}
pub fn run(cli: &Cli, repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    let (mut force, mut delete, mut annotated) = (false, false, false);
    let mut message = None;
    let mut values = Vec::new();
    let mut items = args.iter();
    let mut end = false;
    while let Some(a) = items.next() {
        if end {
            values.push(a.as_str());
            continue;
        }
        match a.as_str() {
            "--" => end = true,
            "-f" | "--force" => force = true,
            "-d" | "--delete" => delete = true,
            "-a" | "--annotate" => annotated = true,
            "-m" | "--message" => {
                annotated = true;
                message = Some(items.next().context("missing tag message")?);
            }
            a if a.starts_with('-') => {
                bail!("unsupported tag option {a}; signers are never executed")
            }
            _ => values.push(a.as_str()),
        }
    }
    ensure!(!values.is_empty(), "tag name required");
    if delete {
        ensure!(!annotated && !force, "incompatible tag options");
        let mut targets = Vec::new();
        for name in values {
            let reference = format!("refs/tags/{name}");
            crate::repo::refname(&reference)?;
            targets.push((
                reference.clone(),
                repo.read_ref(&reference)?.context("tag does not exist")?,
            ));
        }
        for (reference, old) in targets {
            repo.delete_ref(&reference, old)?;
        }
    } else {
        ensure!(values.len() <= 2, "tag takes a name and optional target");
        let name = values[0];
        let reference = format!("refs/tags/{name}");
        crate::repo::refname(&reference)?;
        let old = repo.read_ref(&reference)?;
        ensure!(
            force || old.is_none(),
            "tag already exists; use -f explicitly"
        );
        let target = revision::resolve(repo, store, values.get(1).copied().unwrap_or("HEAD"))?;
        let id = if annotated {
            let message =
                message.context("annotated tags require -m; editors are never executed")?;
            ensure!(!message.trim().is_empty(), "empty tag message");
            let obj = Object {
                kind: Kind::Tag,
                data: format!(
                    "object {target}\ntype {}\ntag {name}\ntagger {}\n\n{}\n",
                    store.get(target)?.kind.name(),
                    porcelain::identity(repo, &cli.config, "COMMITTER")?,
                    message.trim_end_matches('\n')
                )
                .into_bytes(),
            };
            obj.children_for(repo.format)?;
            repo.write_object(&obj)?;
            obj.oid_for(repo.format)?
        } else {
            target
        };
        repo.update_ref(&reference, old, id)?;
    }
    Ok(())
}
