//! Explicit push intent over host-bound endpoints. Repository config cannot redirect it.
use crate::{
    Cli, network,
    object::{self, Kind, Oid, ReadObjects, ancestor},
    pack,
    repo::{Repo, Store},
    revision, trust,
};
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeSet, HashMap, HashSet};

fn receiver_objects(
    store: &mut impl ReadObjects,
    advertisement: &crate::protocol::Advertisement,
) -> Result<HashSet<Oid>> {
    ensure!(
        advertisement.refs.len() <= object::MAX_OBJECTS,
        "too many remote refs"
    );
    let mut pending: Vec<_> = advertisement.refs.values().map(|id| (*id, None)).collect();
    let mut known = HashSet::new();
    let mut bytes = 0usize;
    while let Some((id, expected)) = pending.pop() {
        if !known.insert(id) {
            continue;
        }
        ensure!(
            known.len() <= object::MAX_OBJECTS,
            "remote object count exceeds limit"
        );
        // A verified tree proves the receiver has its blobs. Reading their
        // contents would defeat incremental pushes from partial/large repos.
        if expected == Some(Kind::Blob) {
            continue;
        }
        // Advertised objects may be absent or unreadable locally. They remain
        // known, but only verified local metadata can prove their descendants.
        let Ok(obj) = store.get(id) else {
            continue;
        };
        ensure!(
            expected.is_none_or(|kind| obj.kind == kind),
            "object {id} has unexpected type"
        );
        bytes = bytes.checked_add(obj.data.len()).context("size overflow")?;
        ensure!(
            bytes <= object::MAX_TOTAL,
            "remote metadata exceeds byte limit"
        );
        let mut children = obj.children_for(id.format())?;
        if obj.kind == Kind::Commit && store.shallow(id) {
            children.retain(|(_, kind)| *kind != Kind::Commit);
        }
        ensure!(
            pending.len() + children.len() <= object::MAX_OBJECTS * 4,
            "remote graph fanout exceeds limit"
        );
        pending.extend(children.into_iter().map(|(id, kind)| (id, Some(kind))));
    }
    Ok(known)
}

fn pack_for(
    store: &mut Store<'_>,
    tips: &[Oid],
    advertisement: &crate::protocol::Advertisement,
) -> Result<Vec<u8>> {
    if tips.is_empty() {
        return Ok(Vec::new());
    }
    // Missing receiver metadata is only an optimization opportunity, never a
    // reason to hydrate history from a promisor. Restore normal reads for new
    // objects, which must still pass the existing integrity and size limits.
    let allow = store.allow_promisor;
    store.allow_promisor = false;
    let known = receiver_objects(store, advertisement);
    store.allow_promisor = allow;
    let ids = object::reachable_many_since(store, tips, &known?)?;
    let objects = ids
        .into_iter()
        .map(|id| store.get(id))
        .collect::<Result<Vec<_>>>()?;
    pack::encode_for(&objects, store.format())
}

pub struct Options<'a> {
    pub all: bool,
    pub atomic: bool,
    pub dry_run: bool,
    pub url: Option<&'a str>,
    pub specs: &'a [String],
    pub upstream: bool,
    pub force: bool,
    pub lease: Option<&'a str>,
    pub delete: bool,
    pub tags: bool,
}
fn reference(value: &str) -> Result<String> {
    let name = if value.starts_with("refs/") {
        value.into()
    } else {
        format!("refs/heads/{}", crate::repo::branch(value)?)
    };
    crate::repo::refname(&name)?;
    ensure!(
        name.starts_with("refs/heads/") || name.starts_with("refs/tags/"),
        "push is limited to heads and tags"
    );
    Ok(name)
}
pub async fn run(
    cli: &Cli,
    repo: &Repo,
    store: &mut Store<'_>,
    options: Options<'_>,
) -> Result<()> {
    let endpoint = network::normalize_from(
        &trust::endpoint(repo, options.url, cli.remote_url.as_deref())?,
        &cli.repository,
    )?;
    let mut binding = trust::selected(repo, options.url)?;
    let head = repo.head()?;
    let default = head.strip_prefix("refs/heads/");
    let mut specs = options.specs.to_vec();
    if options.all {
        ensure!(specs.is_empty(), "--all takes no refspecs");
        specs.extend(
            repo.refs()?
                .into_keys()
                .filter(|r| r.starts_with("refs/heads/")),
        );
        if specs.is_empty() {
            return Ok(());
        }
    }
    ensure!(
        !options.delete || !specs.is_empty(),
        "push --delete requires ref names"
    );
    if options.tags {
        ensure!(
            !options.delete && !options.upstream,
            "--tags is incompatible with deletion or upstream"
        );
        specs.extend(
            repo.refs()?
                .into_keys()
                .filter(|r| r.starts_with("refs/tags/")),
        );
    }
    if options.tags && specs.is_empty() {
        return Ok(());
    }
    if specs.is_empty() {
        let local = default.context("detached HEAD requires an explicit refspec")?;
        let dest = binding
            .as_ref()
            .filter(|b| b.url == endpoint)
            .and_then(|b| b.upstreams.get(local))
            .map(String::as_str)
            .unwrap_or(local);
        specs.push(format!("{local}:{dest}"));
    }
    let mut updates = Vec::new();
    let mut destinations = BTreeSet::new();
    for spec in specs {
        let (spec, force) = if let Some(v) = spec.strip_prefix('+') {
            (v, true)
        } else {
            (spec.as_str(), options.force)
        };
        let (local, dest) = spec.split_once(':').unwrap_or((spec, spec));
        let deleting = options.delete || local.is_empty();
        let mut dest = reference(dest)?;
        let source = if local == "HEAD" {
            "HEAD".into()
        } else if local.starts_with("refs/") {
            local.to_owned()
        } else if !local.is_empty()
            && repo.read_ref(&format!("refs/tags/{local}"))?.is_some()
            && repo.read_ref(&format!("refs/heads/{local}"))?.is_none()
        {
            format!("refs/tags/{local}")
        } else {
            reference(local).unwrap_or_else(|_| local.into())
        };
        if !spec.contains(':') && source.starts_with("refs/tags/") {
            dest = source.clone();
        }
        let tip = if deleting {
            repo.format.zero()
        } else {
            revision::resolve(repo, store, &source)?
        };
        if !tip.is_zero() && dest.starts_with("refs/heads/") {
            ensure!(
                store.get(tip)?.kind == Kind::Commit,
                "branch push requires a commit"
            );
        }
        ensure!(
            destinations.insert(dest.clone()),
            "duplicate push destination"
        );
        ensure!(
            !options.upstream
                || (!deleting
                    && source.starts_with("refs/heads/")
                    && dest.starts_with("refs/heads/")),
            "upstream requires local and remote branches"
        );
        updates.push((source, dest, tip, force));
    }
    let auth = cli.auth()?;
    let mut planned = Vec::new();
    for (source, dest, tip, force) in updates {
        let push = network::Push::prepare(&endpoint, &auth, &dest).await?;
        ensure!(
            push.advertisement().format() == repo.format,
            "push object format mismatch"
        );
        ensure!(
            !options.atomic || push.advertisement().caps.contains("atomic"),
            "server does not support atomic push"
        );
        let old = push.old();
        push.unchanged().await?;
        if let Some(lease) = options.lease {
            let expected = if lease.is_empty() {
                let b = binding
                    .as_ref()
                    .filter(|b| b.url == endpoint)
                    .context("lease needs a host-registered remote or explicit ref:OID")?;
                let branch = dest
                    .strip_prefix("refs/heads/")
                    .context("tag lease requires explicit ref:OID")?;
                repo.read_ref(&crate::repo::tracking(&b.remote, branch)?)?
            } else {
                let (name, id) = lease
                    .split_once(':')
                    .context("explicit lease requires ref:OID")?;
                ensure!(reference(name)? == dest, "lease is for another ref");
                if id.is_empty() {
                    None
                } else {
                    Some(Oid::parse(id.as_bytes())?)
                }
            };
            ensure!(
                old == expected,
                "force-with-lease rejected: remote ref changed"
            );
        }
        if let Some(old) = old.filter(|_| !tip.is_zero())
            && old != tip
            && !force
            && options.lease.is_none()
        {
            ensure!(
                dest.starts_with("refs/heads/"),
                "tag already exists; use --force to replace it"
            );
            if store.get(old).is_err() {
                store.add(
                    network::fetch(
                        &endpoint,
                        &auth,
                        &HashMap::from([(dest.clone(), old)]),
                        &[tip],
                    )
                    .await?,
                )?;
            }
            ensure!(
                ancestor(store, old, tip)?,
                "non-fast-forward push refused; destination is unchanged"
            );
        }
        planned.push((source, dest, tip, old));
    }
    if options.atomic && !options.dry_run {
        let updates: Vec<_> = planned
            .iter()
            .filter(|(_, _, tip, old)| *old != Some(*tip) && !(old.is_none() && tip.is_zero()))
            .map(|(_, name, new, old)| crate::protocol::RefUpdate {
                name: name.clone(),
                old: *old,
                new: *new,
            })
            .collect();
        if !updates.is_empty() {
            let push = network::Push::prepare(&endpoint, &auth, &updates[0].name).await?;
            ensure!(
                push.advertisement().format() == repo.format,
                "push object format mismatch"
            );
            let tips: Vec<_> = updates
                .iter()
                .map(|u| u.new)
                .filter(|id| !id.is_zero())
                .collect();
            let pack = pack_for(store, &tips, push.advertisement())?;
            push.send_many(&updates, true, pack).await?;
        }
    }
    for (source, dest, tip, old) in planned {
        if options.dry_run {
            if !cli.quiet {
                eprintln!(
                    "Would push {} -> {}{}",
                    crate::compat::text(&source),
                    crate::compat::text(&dest),
                    if tip.is_zero() { " (delete)" } else { "" }
                );
            }
            continue;
        }
        if !options.atomic && old != Some(tip) && !(old.is_none() && tip.is_zero()) {
            let push = network::Push::prepare(&endpoint, &auth, &dest).await?;
            ensure!(
                push.advertisement().format() == repo.format,
                "push object format mismatch"
            );
            ensure!(
                push.old() == old,
                "push rejected: destination changed while preparing transfer; retry"
            );
            let tips = if tip.is_zero() {
                &[][..]
            } else {
                std::slice::from_ref(&tip)
            };
            let pack = pack_for(store, tips, push.advertisement())?;
            push.send(tip, pack).await?;
        }
        if options.upstream {
            let b = binding.get_or_insert_with(|| trust::Binding {
                filter: None,
                url: endpoint.clone(),
                remote: options
                    .url
                    .filter(|v| !v.contains([':', '/']))
                    .unwrap_or("origin")
                    .into(),
                branch: source.trim_start_matches("refs/heads/").into(),
                single: false,
                mirror: false,
                bare: repo.worktree.is_none(),
                upstreams: Default::default(),
            });
            if b.url != endpoint {
                b.upstreams.clear();
            }
            b.url = endpoint.clone();
            b.upstreams.insert(
                source.trim_start_matches("refs/heads/").into(),
                dest.trim_start_matches("refs/heads/").into(),
            );
            trust::save(repo, b)?;
        }
        if let Some(b) = binding.as_ref().filter(|b| b.url == endpoint && !b.bare)
            && let Some(branch) = dest.strip_prefix("refs/heads/")
        {
            let reference = crate::repo::tracking(&b.remote, branch)?;
            let previous = repo.read_ref(&reference)?;
            if tip.is_zero() {
                if let Some(previous) = previous {
                    repo.delete_ref(&reference, previous)?;
                }
            } else {
                repo.update_ref(&reference, previous, tip)?;
            }
        }
        if !cli.quiet {
            if old == Some(tip) {
                eprintln!("Everything up-to-date");
            } else if tip.is_zero() {
                eprintln!("Deleted {}", crate::compat::text(&dest));
            } else {
                eprint!(
                    "{}",
                    crate::output::push(
                        &endpoint,
                        source.trim_start_matches("refs/heads/"),
                        dest.trim_start_matches("refs/heads/"),
                        old,
                        tip
                    )
                );
            }
        }
    }
    Ok(())
}
