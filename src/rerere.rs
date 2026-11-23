//! Explicit reuse of recorded resolutions. Cache entries are checked objects,
//! never scripts; applying one changes the worktree but never stages it.
use crate::{
    Cli, checkout,
    index::Entry,
    object::{Kind, Oid, ReadObjects},
    porcelain,
    repo::{Repo, Store},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
const CACHE: &str = "safe-git-rerere";
#[derive(Default, Serialize, Deserialize)]
struct Cache {
    records: BTreeMap<String, Record>,
    active: Vec<(Vec<u8>, String)>,
}
#[derive(Serialize, Deserialize)]
struct Record {
    pre: Entry,
    post: Option<Entry>,
}
pub fn roots(repo: &Repo) -> Result<Vec<Oid>> {
    let cache = load(repo)?;
    Ok(cache
        .records
        .values()
        .flat_map(|r| std::iter::once(r.pre.oid).chain(r.post.map(|e| e.oid)))
        .collect())
}
fn load(repo: &Repo) -> Result<Cache> {
    let cache: Cache = repo
        .read(CACHE, 8 * 1024 * 1024)?
        .map(|b| serde_json::from_slice(&b))
        .transpose()?
        .unwrap_or_default();
    ensure!(
        cache.records.len() <= 10000 && cache.active.len() <= 10000,
        "rerere cache exceeds limit"
    );
    for (path, key) in &cache.active {
        crate::index::path(path)?;
        ensure!(cache.records.contains_key(key), "invalid active resolution");
    }
    for r in cache.records.values() {
        for e in std::iter::once(r.pre).chain(r.post) {
            ensure!(
                matches!(e.mode, 0o100644 | 0o100755),
                "rerere supports regular files"
            );
        }
    }
    Ok(cache)
}
fn markers(bytes: &[u8]) -> bool {
    bytes.split(|b| *b == b'\n').any(|l| {
        l.starts_with(b"<<<<<<<") || l.starts_with(b"=======") || l.starts_with(b">>>>>>>")
    })
}
pub fn run(_cli: &Cli, repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    let action = args.first().map(String::as_str).unwrap_or("");
    ensure!(
        matches!(
            action,
            "" | "status" | "remaining" | "diff" | "clear" | "forget"
        ),
        "unsupported rerere command"
    );
    let mut cache = load(repo)?;
    if action == "clear" {
        cache.active.clear();
    } else if action == "forget" {
        ensure!(args.len() == 2, "rerere forget requires a path");
        let path = crate::workflow::paths(&[args[1].clone()], &_cli.prefix)?.remove(0);
        if let Some((_, key)) = cache.active.iter().find(|(p, _)| p == &path) {
            cache.records.remove(&key.clone());
        }
        cache.active.retain(|(p, _)| p != &path);
    } else {
        ensure!(args.len() <= 1, "unexpected rerere arguments");
        let (index, conflicts) = porcelain::index_full(repo)?;
        let mut tracked = index.clone();
        for (path, stages) in &conflicts {
            if let Some(e) = stages
                .iter()
                .flatten()
                .find(|e| matches!(e.mode, 0o100644 | 0o100755))
            {
                tracked.insert(path.clone(), *e);
            }
        }
        for (path, key) in &cache.active {
            tracked
                .entry(path.clone())
                .or_insert(cache.records[key].pre);
        }
        let before = checkout::snapshot(repo, &tracked)?;
        let mut after = before.clone();
        for (path, stages) in &conflicts {
            let Some(e) = before
                .get(path)
                .copied()
                .filter(|e| matches!(e.mode, 0o100644 | 0o100755))
            else {
                continue;
            };
            let bytes = serde_json::to_vec(stages)?;
            let key = crate::object::hash_for(repo.format, &[&bytes])?.to_string();
            if !cache.records.contains_key(&key) {
                let object = store.get(e.oid)?;
                if object.kind != Kind::Blob || !markers(&object.data) {
                    continue;
                }
                cache
                    .records
                    .insert(key.clone(), Record { pre: e, post: None });
            }
            cache.active.retain(|(p, _)| p != path);
            cache.active.push((path.clone(), key));
        }
        for (path, key) in &cache.active {
            let record = cache
                .records
                .get_mut(key)
                .context("missing resolution record")?;
            let Some(actual) = before.get(path) else {
                continue;
            };
            if action == "status" || action == "remaining" {
                if action == "status" || record.post.is_none() {
                    println!("{}", crate::workflow::quote_path(path));
                }
            } else if action == "diff" {
                let a = store.get(record.pre.oid)?;
                let b = store.get(actual.oid)?;
                crate::compat::diff_budget(&a.data, &b.data)?;
                println!(
                    "{}",
                    crate::compat::text(&String::from_utf8_lossy(
                        &diffy::create_patch_bytes(&a.data, &b.data).to_bytes()
                    ))
                );
            } else if action.is_empty() {
                if let Some(post) = record.post {
                    let object = store.get(post.oid)?;
                    ensure!(object.kind == Kind::Blob, "resolution is not a blob");
                    if *actual == record.pre {
                        after.insert(path.clone(), post);
                    }
                } else if *actual != record.pre {
                    let object = store.get(actual.oid)?;
                    ensure!(object.kind == Kind::Blob, "resolution is not a blob");
                    if !markers(&object.data) {
                        record.post = Some(*actual);
                    }
                }
            }
        }
        if before != after {
            checkout::edit(
                repo,
                store,
                checkout::Edit {
                    head: repo.head()?,
                    tip: repo.read_ref("HEAD")?.context("unborn HEAD")?,
                    before,
                    after,
                    index_before: index.clone(),
                    index_after: index,
                    conflicts_before: conflicts.clone(),
                    conflicts_after: conflicts,
                },
            )?;
        }
    }
    let bytes = serde_json::to_vec(&cache)?;
    ensure!(bytes.len() <= 8 * 1024 * 1024, "rerere cache exceeds limit");
    repo.atomic(CACHE, &bytes)
}
