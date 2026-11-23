//! Remote names and endpoints are registered by the caller in trusted host state.
use crate::{
    Cli, compat, network,
    repo::{Repo, Store},
    trust,
};
use anyhow::{Context, Result, bail, ensure};
pub async fn run(cli: &Cli, repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    let mut registry = trust::registry(repo)?;
    let command = args.first().map(String::as_str).unwrap_or("");
    match command {
        "" | "-v" | "--verbose" => {
            ensure!(args.len() <= 1, "unexpected remote arguments");
            for b in registry.remotes.values() {
                if command.is_empty() {
                    println!("{}", b.remote);
                } else {
                    println!(
                        "{}\t{} (fetch)\n{}\t{} (push)",
                        b.remote,
                        compat::text(&b.url),
                        b.remote,
                        compat::text(&b.url)
                    );
                }
            }
        }
        "add" | "set-url" => {
            ensure!(args.len() == 3, "remote {command} requires NAME URL");
            let name = &args[1];
            crate::repo::tracking(name, "main")?;
            if command == "add" {
                ensure!(
                    !registry.remotes.contains_key(name),
                    "remote already exists"
                );
            } else {
                ensure!(
                    registry.remotes.contains_key(name),
                    "remote is not registered"
                );
            }
            let mut binding = registry.remotes.remove(name).unwrap_or(trust::Binding {
                filter: None,
                remote: name.clone(),
                url: String::new(),
                branch: repo
                    .head()?
                    .strip_prefix("refs/heads/")
                    .unwrap_or("main")
                    .into(),
                single: false,
                mirror: false,
                bare: repo.worktree.is_none(),
                upstreams: Default::default(),
            });
            let url = network::normalize_from(&args[2], &cli.repository)?;
            if binding.url != url {
                binding.upstreams.clear();
            }
            binding.url = url;
            trust::save(repo, &binding)?;
        }
        "get-url" => {
            ensure!(args.len() == 2, "get-url requires NAME");
            println!(
                "{}",
                compat::text(
                    &registry
                        .remotes
                        .get(&args[1])
                        .context("remote is not registered")?
                        .url
                )
            );
        }
        "remove" | "rm" | "rename" => {
            ensure!(
                args.len() == if command == "rename" { 3 } else { 2 },
                "invalid remote arguments"
            );
            let old = &args[1];
            let mut binding = registry
                .remotes
                .remove(old)
                .context("remote is not registered")?;
            let prefix = format!("refs/remotes/{old}/");
            let refs: Vec<_> = repo
                .refs()?
                .into_iter()
                .filter(|(r, _)| r.starts_with(&prefix))
                .collect();
            if command == "rename" {
                let new = &args[2];
                crate::repo::tracking(new, "main")?;
                ensure!(!registry.remotes.contains_key(new), "remote already exists");
                for (r, _) in &refs {
                    let target = format!("refs/remotes/{new}/{}", &r[prefix.len()..]);
                    ensure!(
                        repo.read_ref(&target)?.is_none(),
                        "destination tracking ref exists"
                    );
                }
                for (r, id) in &refs {
                    let target = format!("refs/remotes/{new}/{}", &r[prefix.len()..]);
                    if let Some(bytes) = repo.read(r, 2048)?
                        && let Some(sym) = bytes.strip_prefix(b"ref: ")
                    {
                        let sym = std::str::from_utf8(sym)?.trim_end_matches('\n');
                        let suffix = sym
                            .strip_prefix(&prefix)
                            .context("remote symbolic ref leaves its namespace")?;
                        repo.atomic(
                            &target,
                            format!("ref: refs/remotes/{new}/{suffix}\n").as_bytes(),
                        )?;
                    } else {
                        repo.update_ref(&target, None, *id)?;
                    }
                }
                binding.remote = new.clone();
                registry.remotes.insert(new.clone(), binding);
                if &registry.default == old {
                    registry.default = new.clone();
                }
            } else if &registry.default == old {
                registry.default = registry.remotes.keys().next().cloned().unwrap_or_default();
            }
            // Remove symbolic refs before their targets.
            for (r, id) in refs.iter().rev() {
                if repo.read_ref(r)?.is_some() {
                    repo.delete_ref(r, *id)?;
                } else {
                    repo.remove(r)?;
                }
            }
            trust::save_registry(repo, &registry)?;
        }
        "show" => {
            let values: Vec<_> = args[1..].iter().filter(|a| a.as_str() != "-n").collect();
            ensure!(values.len() == 1, "remote show requires NAME");
            let b = registry
                .remotes
                .get(values[0])
                .context("remote is not registered")?;
            println!(
                "* remote {}\n  Fetch URL: {}\n  Push URL: {}",
                b.remote,
                compat::text(&b.url),
                compat::text(&b.url)
            );
            if !args.iter().any(|a| a == "-n") {
                let ad = network::discover(&b.url, &cli.auth()?).await?;
                for r in ad.refs.keys().filter(|r| r.starts_with("refs/heads/")) {
                    println!("  {}", compat::text(r));
                }
            }
        }
        "update" => {
            for b in registry
                .remotes
                .values()
                .filter(|b| args.len() == 1 || args[1..].contains(&b.remote))
            {
                compat::fetch_binding(cli, repo, store, false, b).await?;
            }
        }
        _ => bail!("unsupported remote command {command}"),
    }
    Ok(())
}
