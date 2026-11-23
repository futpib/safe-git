//! Static Git HTTP transport. Only fixed metadata paths and verified object IDs
//! select requests; alternates and server-provided URLs are never followed.
use crate::{
    network::{self, Fetched},
    object::{MAX_OBJECT, MAX_OBJECTS, MAX_PACK, MAX_TOTAL, Objects, Oid},
    protocol::Advertisement,
};
use anyhow::{Context, Result, ensure};
use std::collections::{HashMap, HashSet};

async fn get(
    client: &reqwest::Client,
    base: &url::Url,
    path: &str,
    limit: usize,
) -> Result<Option<Vec<u8>>> {
    let response = network::http_auth(client.get(network::service_url(base, path)?), base)?
        .send()
        .await?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    Ok(Some(network::read_body(response, limit).await?))
}

pub async fn advertisement(
    client: &reqwest::Client,
    base: &url::Url,
    bytes: &[u8],
) -> Result<Advertisement> {
    let mut ad = Advertisement::new();
    for line in std::str::from_utf8(bytes)?.lines() {
        ensure!(ad.refs.len() < 100_000, "too many static refs");
        let (id, name) = line.split_once('\t').context("invalid static Git refs")?;
        ensure!(name != "HEAD", "HEAD is not allowed in static info/refs");
        ad.line(format!("{id} {name}\n").as_bytes(), false)?;
    }
    let head = get(client, base, "HEAD", 2048)
        .await?
        .context("missing static HEAD")?;
    let head = std::str::from_utf8(&head)?.trim_end_matches('\n');
    if let Some(name) = head.strip_prefix("ref: ") {
        crate::repo::refname(name)?;
        ensure!(
            name.starts_with("refs/heads/"),
            "static HEAD must name a branch"
        );
        ad.caps.insert(format!("symref=HEAD:{name}"));
        if let Some(id) = ad.refs.get(name).copied() {
            ad.refs.insert("HEAD".into(), id);
        } else {
            ensure!(ad.refs.is_empty(), "static HEAD names a missing branch");
        }
    } else {
        let id = Oid::parse(head.as_bytes())?;
        ensure!(id != Oid::ZERO, "zero static HEAD");
        ad.refs.insert("HEAD".into(), id);
    }
    Ok(ad)
}

pub async fn fetch(
    client: &reqwest::Client,
    base: &url::Url,
    refs: &HashMap<String, Oid>,
    options: &network::FetchOptions,
) -> Result<Fetched> {
    ensure!(
        !options.changes_depth() && options.shallow.is_empty(),
        "static HTTP cannot negotiate shallow history; use a smart Git endpoint"
    );
    ensure!(
        options.filter.is_none(),
        "static HTTP cannot negotiate object filters; use a smart Git endpoint"
    );
    let mut objects = Objects::new();
    let mut visited = HashSet::new();
    let mut pending: Vec<_> = refs.values().map(|id| (*id, None)).collect();
    let mut packs_loaded = false;
    let mut transferred = 0usize;
    let mut inflated = 0usize;
    while let Some((id, expected)) = pending.pop() {
        if let std::collections::hash_map::Entry::Vacant(entry) = objects.entry(id) {
            let hex = id.to_string();
            if let Some(bytes) = get(
                client,
                base,
                &format!("objects/{}/{}", &hex[..2], &hex[2..]),
                MAX_OBJECT + 1024 * 1024,
            )
            .await?
            {
                transferred += bytes.len();
                ensure!(transferred <= MAX_TOTAL, "static transfer exceeds limit");
                let obj = crate::repo::loose(&bytes)?;
                ensure!(obj.oid()? == id, "static object checksum mismatch");
                inflated += obj.data.len();
                entry.insert(std::sync::Arc::new(obj));
            } else if !packs_loaded {
                packs_loaded = true;
                let list = get(client, base, "objects/info/packs", 1024 * 1024)
                    .await?
                    .unwrap_or_default();
                let mut names = HashSet::new();
                for line in std::str::from_utf8(&list)?
                    .lines()
                    .filter(|l| !l.is_empty())
                {
                    let hash = line
                        .strip_prefix("P pack-")
                        .and_then(|v| v.strip_suffix(".pack"))
                        .context("invalid static pack name")?;
                    let hash = Oid::parse(hash.as_bytes())?;
                    ensure!(
                        names.insert(hash) && names.len() <= 128,
                        "duplicate or excessive static packs"
                    );
                    let bytes = get(
                        client,
                        base,
                        &format!("objects/pack/pack-{hash}.pack"),
                        MAX_PACK,
                    )
                    .await?
                    .context("missing static pack")?;
                    transferred += bytes.len();
                    ensure!(transferred <= MAX_TOTAL, "static transfer exceeds limit");
                    ensure!(
                        bytes.len() >= 20 && bytes[bytes.len() - 20..] == hash.0,
                        "static pack name disagrees with checksum"
                    );
                    for (id, obj) in crate::pack::decode(&bytes)? {
                        if let std::collections::hash_map::Entry::Vacant(entry) = objects.entry(id)
                        {
                            inflated += obj.data.len();
                            entry.insert(obj);
                        }
                    }
                    ensure!(
                        inflated <= MAX_TOTAL && objects.len() <= MAX_OBJECTS,
                        "static object budget exceeded"
                    );
                }
            }
        }
        ensure!(
            inflated <= MAX_TOTAL && objects.len() <= MAX_OBJECTS,
            "static object budget exceeded"
        );
        let obj = objects
            .get(&id)
            .context("missing static object (alternates are never followed)")?;
        ensure!(
            expected.is_none_or(|kind| kind == obj.kind),
            "static object type mismatch"
        );
        if visited.insert(id) {
            pending.extend(
                obj.children()?
                    .into_iter()
                    .map(|(id, kind)| (id, Some(kind))),
            );
            ensure!(
                pending.len() <= MAX_OBJECTS * 4,
                "static traversal exceeds limit"
            );
        }
    }
    // Drop unrelated pack objects. The existing clone/fetch publication performs
    // its own reachability checks before any ref or checkout is installed.
    objects.retain(|id, _| visited.contains(id));
    Ok(Fetched {
        objects,
        shallow: HashSet::new(),
    })
}
