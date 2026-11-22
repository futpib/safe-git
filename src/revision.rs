use crate::{
    config,
    object::{Kind, MAX_OBJECTS, Oid, ReadObjects, commits},
    repo::{Repo, Store},
};
use anyhow::{Context, Result, ensure};
use std::collections::{HashSet, VecDeque};
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
pub fn peel(store: &mut impl ReadObjects, mut id: Oid) -> Result<Oid> {
    for _ in 0..32 {
        let object = store.get(id)?;
        if object.kind != Kind::Tag {
            return Ok(id);
        }
        let (target, kind) = object.children()?[0];
        ensure!(store.get(target)?.kind == kind, "tag target type mismatch");
        id = target;
    }
    anyhow::bail!("tag nesting exceeds limit")
}
pub fn upstream(repo: &Repo) -> Result<String> {
    let head = repo.head()?;
    let branch = head
        .strip_prefix("refs/heads/")
        .context("detached HEAD has no upstream")?;
    let cfg = config::local(repo)?;
    let remote = cfg
        .get(&format!("branch.{branch}.remote"))
        .map(String::as_str)
        .unwrap_or("origin");
    let merge = cfg
        .get(&format!("branch.{branch}.merge"))
        .map(String::as_str)
        .unwrap_or(branch);
    if remote == "." {
        Ok(format!("refs/heads/{}", crate::repo::branch(merge)?))
    } else {
        crate::repo::tracking(remote, merge)
    }
}
pub fn resolve(repo: &Repo, store: &mut Store<'_>, value: &str) -> Result<Oid> {
    ensure!(value.len() <= 4096 && !value.is_empty(), "invalid revision");
    if value == EMPTY_TREE {
        return Oid::parse(value.as_bytes());
    }
    if let Some(base) = value.strip_suffix("^{commit}") {
        let id = resolve(repo, store, base)?;
        let id = peel(store, id)?;
        store.get(id)?.commit()?;
        return Ok(id);
    }
    if let Some(base) = value.strip_suffix("^{tree}") {
        let id = resolve(repo, store, base)?;
        let id = peel(store, id)?;
        let obj = store.get(id)?;
        return Ok(if obj.kind == Kind::Tree {
            id
        } else {
            obj.commit()?.0
        });
    }
    if let Some(base) = value.strip_suffix("^{}") {
        let id = resolve(repo, store, base)?;
        return peel(store, id);
    }
    if let Some((base, suffix)) = value.rsplit_once(['~', '^']) {
        let position = base.len();
        let count = if suffix.is_empty() {
            1
        } else {
            suffix.parse::<usize>().context("invalid parent selector")?
        };
        ensure!(count <= MAX_OBJECTS, "parent traversal exceeds limit");
        let start = resolve(repo, store, base)?;
        let mut id = peel(store, start)?;
        if value.as_bytes()[position] == b'^' {
            if count > 0 {
                id = *store
                    .get(id)?
                    .commit()?
                    .1
                    .get(count - 1)
                    .context("missing parent")?;
            }
        } else {
            for _ in 0..count {
                id = *store
                    .get(id)?
                    .commit()?
                    .1
                    .first()
                    .context("missing parent")?;
            }
        }
        return Ok(id);
    }
    if matches!(
        value,
        "@{u}" | "HEAD@{u}" | "@{upstream}" | "HEAD@{upstream}"
    ) {
        return repo
            .read_ref(&upstream(repo)?)?
            .context("upstream ref does not exist");
    }
    if value == "FETCH_HEAD" {
        let bytes = repo
            .read("FETCH_HEAD", 8 * 1024 * 1024)?
            .context("missing FETCH_HEAD")?;
        return Oid::parse(bytes.get(..40).context("invalid FETCH_HEAD")?);
    }
    if value == "HEAD" || value == "@" {
        return repo.read_ref("HEAD")?.context("unborn HEAD");
    }
    if value == "AUR_SEEN" {
        return repo.read_ref(value)?.context("unknown revision AUR_SEEN");
    }
    if value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        let id = Oid::parse(value.as_bytes())?;
        store.get(id)?;
        return Ok(id);
    }
    let candidates = if value.starts_with("refs/") {
        vec![value.to_owned()]
    } else {
        vec![
            format!("refs/heads/{value}"),
            format!("refs/tags/{value}"),
            format!("refs/remotes/{value}"),
            format!("refs/remotes/{value}/HEAD"),
        ]
    };
    let mut found = None;
    for name in candidates {
        crate::repo::refname(&name)?;
        let id = match repo.read_ref(&name) {
            Ok(id) => id,
            Err(e) if e.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOTDIR) => {
                None
            }
            Err(e) => return Err(e),
        };
        if let Some(id) = id {
            ensure!(found.is_none(), "ambiguous revision {value}");
            found = Some(id);
        }
    }
    if let Some(id) = found {
        return Ok(id);
    }
    if (4..40).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        let matches = store
            .ids()?
            .into_iter()
            .filter(|id| id.to_string().starts_with(&value.to_ascii_lowercase()))
            .collect::<Vec<_>>();
        ensure!(
            matches.len() == 1,
            "unknown or ambiguous abbreviated object ID"
        );
        store.get(matches[0])?;
        return Ok(matches[0]);
    }
    anyhow::bail!("unknown revision {value}")
}
pub fn commit(repo: &Repo, store: &mut Store<'_>, value: &str) -> Result<Oid> {
    let id = resolve(repo, store, value)?;
    let id = peel(store, id)?;
    store.get(id)?.commit()?;
    Ok(id)
}
pub fn history(
    store: &mut impl ReadObjects,
    tip: Oid,
    exclude: Option<Oid>,
    first_parent: bool,
) -> Result<Vec<Oid>> {
    let excluded = exclude
        .map(|id| commits(store, &[id]))
        .transpose()?
        .unwrap_or_default();
    let mut seen = HashSet::new();
    let mut todo = VecDeque::from([tip]);
    let mut out = Vec::new();
    while let Some(id) = todo.pop_front() {
        if excluded.contains(&id) || !seen.insert(id) {
            continue;
        }
        ensure!(seen.len() <= MAX_OBJECTS, "history exceeds limit");
        out.push(id);
        let parents = store.get(id)?.commit()?.1;
        todo.extend(
            parents
                .into_iter()
                .take(if first_parent { 1 } else { usize::MAX }),
        );
        ensure!(
            todo.len() <= MAX_OBJECTS * 4,
            "history fanout exceeds limit"
        );
    }
    Ok(out)
}
pub fn merge_base(store: &mut impl ReadObjects, a: Oid, b: Oid) -> Result<Oid> {
    let left = commits(store, &[a])?;
    let right = commits(store, &[b])?;
    let common: HashSet<_> = left.intersection(&right).copied().collect();
    let mut ancestors = HashSet::new();
    for id in &common {
        for parent in store.get(*id)?.commit()?.1 {
            if common.contains(&parent) {
                ancestors.insert(parent);
            }
        }
    }
    let best: Vec<_> = common.difference(&ancestors).copied().collect();
    ensure!(
        best.len() == 1,
        "unrelated histories or multiple merge bases are unsupported"
    );
    Ok(best[0])
}
