//! Partial-clone promises bind only to caller-authorized host records, never .git/config.
use crate::{
    Cli, network,
    object::{Kind, MAX_OBJECTS, MAX_TOTAL, Object, Objects, Oid, ReadObjects},
    repo::{MissingObject, Repo, Store},
    transport::Auth,
    trust,
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{HashMap, HashSet},
    sync::OnceLock,
    time::{Duration, Instant},
};
struct ContextState {
    auth: Auth,
    deadline: Instant,
}
static CONTEXT: OnceLock<ContextState> = OnceLock::new();
pub fn configure(cli: &Cli) {
    if let Ok(auth) = cli.auth() {
        let _ = CONTEXT.set(ContextState {
            auth,
            deadline: Instant::now() + Duration::from_secs(cli.timeout),
        });
    }
}
pub fn filter(value: &str) -> Result<Option<u64>> {
    if value == "blob:none" {
        return Ok(None);
    }
    let number = value
        .strip_prefix("blob:limit=")
        .context("supported filters are blob:none and blob:limit=N")?;
    let (number, scale) = match number.as_bytes().last() {
        Some(b'k' | b'K') => (&number[..number.len() - 1], 1024u64),
        Some(b'm' | b'M') => (&number[..number.len() - 1], 1024u64.pow(2)),
        Some(b'g' | b'G') => (&number[..number.len() - 1], 1024u64.pow(3)),
        _ => (number, 1),
    };
    ensure!(
        !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()),
        "invalid blob limit"
    );
    Ok(Some(
        number
            .parse::<u64>()?
            .checked_mul(scale)
            .context("blob limit overflow")?,
    ))
}
pub fn omitted(value: &str, obj: &Object) -> Result<bool> {
    Ok(obj.kind == Kind::Blob && filter(value)?.is_none_or(|limit| obj.data.len() as u64 >= limit))
}
pub fn store_pack(repo: &Repo, objects: &Objects) -> Result<()> {
    let mut objects: Vec<_> = objects.iter().collect();
    objects.sort_by_key(|(id, _)| **id);
    let objects: Vec<_> = objects.into_iter().map(|(_, o)| o.clone()).collect();
    let (pack, index) = crate::pack::encode_indexed(&objects, true)?;
    let id = Oid(pack[pack.len() - 20..].try_into()?);
    let path = format!("objects/pack/pack-{id}");
    repo.atomic(&format!("{path}.idx"), &index)?;
    repo.atomic(&format!("{path}.promisor"), b"")?;
    repo.atomic(&format!("{path}.pack"), &pack)
}
/// Persist reachable verified objects, allowing only blobs promised by an explicit filter.
pub fn persist(
    repo: &Repo,
    store: &mut Store<'_>,
    tips: &[Oid],
    filtered: bool,
    boundary: &HashSet<Oid>,
) -> Result<HashSet<Oid>> {
    let allow = store.allow_promisor;
    store.allow_promisor = false;
    let result = (|| {
        let mut seen = HashSet::new();
        let mut kinds = HashMap::new();
        let mut todo = Vec::new();
        let mut total = 0;
        for id in tips {
            todo.push((*id, store.get(*id)?.kind));
        }
        while let Some((id, kind)) = todo.pop() {
            if let Some(previous) = kinds.insert(id, kind) {
                ensure!(previous == kind, "object referenced with conflicting types");
            }
            if kind == Kind::Commit && boundary.contains(&id) {
                continue;
            }
            if !seen.insert(id) {
                continue;
            }
            ensure!(
                seen.len() <= MAX_OBJECTS && todo.len() <= MAX_OBJECTS * 4,
                "partial traversal exceeds limit"
            );
            let object = match store.get(id) {
                Ok(o) => o,
                Err(e)
                    if filtered
                        && kind == Kind::Blob
                        && e.downcast_ref::<MissingObject>().is_some() =>
                {
                    continue;
                }
                Err(e) => return Err(e),
            };
            ensure!(object.kind == kind, "object type mismatch");
            total += object.data.len();
            ensure!(total <= MAX_TOTAL, "partial object budget exceeded");
            for (child, expected) in object.children()? {
                if !(kind == Kind::Commit
                    && expected == Kind::Commit
                    && store.shallow.contains(&id))
                {
                    todo.push((child, expected));
                }
            }
            repo.write_object(&object)?;
        }
        Ok(seen)
    })();
    store.allow_promisor = allow;
    result
}
pub fn boundary(
    store: &mut Store<'_>,
    haves: &[Oid],
    options: &network::FetchOptions,
) -> Result<HashSet<Oid>> {
    if options.changes_depth() || options.filter.is_some() || !options.shallow.is_empty() {
        Ok(HashSet::new())
    } else {
        crate::object::commits(store, haves)
    }
}
pub fn hydrate(repo: &Repo, id: Oid) -> Result<Option<Objects>> {
    let Some(context) = CONTEXT.get() else {
        return Ok(None);
    };
    let registry = trust::registry(repo)?;
    let Some(binding) = registry.remotes.values().find(|b| b.filter.is_some()) else {
        return Ok(None);
    };
    filter(binding.filter.as_deref().unwrap())?;
    let runtime = tokio::runtime::Handle::try_current()
        .context("partial fetch requires the command runtime")?;
    let remaining = context
        .deadline
        .checked_duration_since(Instant::now())
        .context("operation timed out")?;
    let objects = tokio::task::block_in_place(|| {
        runtime.block_on(async {
            let options = network::FetchOptions {
                raw: true,
                ..Default::default()
            };
            tokio::time::timeout(
                remaining,
                network::fetch_options(
                    &binding.url,
                    &context.auth,
                    &HashMap::from([(id.to_string(), id)]),
                    &[],
                    &options,
                ),
            )
            .await
            .context("partial fetch timed out")?
        })
    })?
    .objects;
    ensure!(
        objects.contains_key(&id),
        "promisor did not send the requested object"
    );
    for object in objects.values() {
        repo.write_object(object)?;
    }
    Ok(Some(objects))
}
