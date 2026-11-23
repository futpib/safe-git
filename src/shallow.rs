use crate::{
    object::{MAX_OBJECTS, Oid, ReadObjects},
    repo::{Repo, Store},
};
use anyhow::{Result, ensure};
use std::collections::{HashSet, VecDeque};
pub fn read(repo: &Repo) -> Result<HashSet<Oid>> {
    let mut result = HashSet::new();
    if let Some(bytes) = repo.read("shallow", MAX_OBJECTS * 41)? {
        ensure!(
            !bytes.is_empty() && bytes.ends_with(b"\n"),
            "invalid shallow boundaries"
        );
        for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            let id = Oid::parse(line)?;
            ensure!(
                id != Oid::ZERO && result.insert(id),
                "invalid or duplicate shallow boundary"
            );
        }
    }
    Ok(result)
}
pub fn save(repo: &Repo, boundary: &HashSet<Oid>) -> Result<()> {
    if boundary.is_empty() {
        return repo.remove("shallow");
    }
    let mut ids: Vec<_> = boundary.iter().collect();
    ids.sort();
    repo.atomic(
        "shallow",
        ids.iter()
            .map(|id| format!("{id}\n"))
            .collect::<String>()
            .as_bytes(),
    )
}
pub fn boundary(store: &mut Store<'_>, tips: &[Oid], depth: u32) -> Result<HashSet<Oid>> {
    ensure!(depth > 0, "depth must be positive");
    let mut todo: VecDeque<_> = tips.iter().map(|id| (*id, 1u32)).collect();
    let mut seen = HashSet::new();
    let mut out = HashSet::new();
    while let Some((id, n)) = todo.pop_front() {
        if !seen.insert(id) {
            continue;
        }
        ensure!(
            seen.len() <= MAX_OBJECTS && todo.len() <= MAX_OBJECTS * 4,
            "shallow traversal exceeds limit"
        );
        let parents = store.parents(id)?;
        if n >= depth && !parents.is_empty() {
            out.insert(id);
        } else {
            todo.extend(parents.into_iter().map(|p| (p, n + 1)));
        }
    }
    Ok(out)
}
