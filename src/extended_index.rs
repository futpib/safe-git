//! Expand verified split and sparse indexes into ordinary entries before any edits.
use crate::{
    index::{self, Conflicts, Entries, Records, SKIP},
    object::{MAX_OBJECTS, Oid},
    repo::{Repo, Store},
};
use anyhow::{Context, Result, ensure};
use std::collections::BTreeSet;
fn u32_at(bytes: &[u8], pos: &mut usize) -> Result<usize> {
    let n = u32::from_be_bytes(
        bytes
            .get(*pos..*pos + 4)
            .context("truncated bitmap")?
            .try_into()?,
    ) as usize;
    *pos += 4;
    Ok(n)
}
fn bitmap(bytes: &[u8], pos: &mut usize, limit: usize) -> Result<BTreeSet<usize>> {
    let bits = u32_at(bytes, pos)?;
    let words = u32_at(bytes, pos)?;
    ensure!(
        bits <= limit && words <= MAX_OBJECTS,
        "split index bitmap exceeds limit"
    );
    let data = bytes
        .get(*pos..*pos + words * 8)
        .context("truncated bitmap words")?;
    *pos += words * 8;
    let last = u32_at(bytes, pos)?;
    ensure!(words > 0 && last < words, "invalid bitmap run pointer");
    let mut at = 0;
    let mut bit = 0usize;
    let mut set = BTreeSet::new();
    let mut final_run = 0;
    while at < words {
        final_run = at;
        let run = u64::from_be_bytes(data[at * 8..at * 8 + 8].try_into()?);
        at += 1;
        let repeated = ((run >> 1) & 0xffff_ffff) as usize;
        let literals = (run >> 33) as usize;
        ensure!(
            repeated <= MAX_OBJECTS.div_ceil(64) && literals <= words - at,
            "invalid bitmap run"
        );
        let end = bit.checked_add(repeated * 64).context("bitmap overflow")?;
        ensure!(
            end <= limit.div_ceil(64) * 64,
            "bitmap exceeds shared index"
        );
        if run & 1 != 0 {
            for n in bit..end.min(bits) {
                set.insert(n);
            }
        }
        bit = end;
        for _ in 0..literals {
            let word = u64::from_be_bytes(data[at * 8..at * 8 + 8].try_into()?);
            at += 1;
            for n in 0..64 {
                if word & (1u64 << n) != 0 {
                    ensure!(bit + n < bits, "set bit outside declared bitmap");
                    set.insert(bit + n);
                }
            }
            bit += 64;
            ensure!(
                bit <= limit.div_ceil(64) * 64,
                "bitmap exceeds shared index"
            );
        }
    }
    ensure!(
        final_run == last && bit >= bits,
        "invalid bitmap length or run pointer"
    );
    Ok(set)
}
fn ordered(records: &Records) -> Result<()> {
    for pair in records.windows(2) {
        ensure!(
            (&pair[0].0, pair[0].1) < (&pair[1].0, pair[1].1),
            "duplicate or unordered index entries"
        );
    }
    Ok(())
}
pub fn decode(repo: Option<&Repo>, bytes: &[u8]) -> Result<(Entries, Conflicts)> {
    let mut raw = index::decode_raw(bytes)?;
    if let Some(link) = &raw.link {
        let repo = repo.context("split index requires repository context")?;
        ensure!(link.len() >= 20, "truncated split index link");
        let id = Oid(link[..20].try_into()?);
        let base = if id == Oid::ZERO {
            Vec::new()
        } else {
            let data = repo
                .read(&format!("sharedindex.{id}"), crate::checkout::INDEX_LIMIT)?
                .context("missing shared index")?;
            ensure!(
                data.len() >= 20 && data[data.len() - 20..] == id.0,
                "shared index hash mismatch"
            );
            let base = index::decode_raw(&data)?;
            ensure!(
                base.link.is_none() && !base.sparse,
                "nested split indexes are invalid"
            );
            ordered(&base.records)?;
            base.records
        };
        let mut pos = 20;
        let (deleted, replaced) = if link.len() == 20 {
            (BTreeSet::new(), BTreeSet::new())
        } else {
            (
                bitmap(link, &mut pos, base.len())?,
                bitmap(link, &mut pos, base.len())?,
            )
        };
        ensure!(
            pos == link.len() && deleted.is_disjoint(&replaced),
            "invalid split index bitmaps"
        );
        ensure!(
            replaced.len() <= raw.records.len(),
            "split index replacement missing"
        );
        let mut changes = raw.records.into_iter();
        let mut records = Vec::new();
        for (n, entry) in base.into_iter().enumerate() {
            if deleted.contains(&n) {
                continue;
            }
            if replaced.contains(&n) {
                let (name, stage, value) = changes.next().context("replacement missing")?;
                ensure!(
                    name.is_empty() || name == entry.0,
                    "split replacement name differs"
                );
                records.push((entry.0, stage, value));
            } else {
                records.push(entry);
            }
        }
        let additions: Records = changes.collect();
        ordered(&additions)?;
        records.extend(additions);
        records.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
        raw.records = records;
    }
    ordered(&raw.records)?;
    let mut entries = Entries::new();
    let mut conflicts = Conflicts::new();
    for (name, stage, entry) in raw.records {
        if entry.mode == 0o40000 {
            ensure!(
                raw.sparse && stage == 0 && entry.flags == SKIP && name.ends_with(b"/"),
                "invalid sparse directory entry"
            );
            index::path(&name[..name.len() - 1])?;
            let repo = repo.context("sparse index requires repository context")?;
            let mut store = Store::new(repo);
            store.allow_promisor = false;
            for (path, mut value) in index::tree(&mut store, Some(entry.oid))? {
                let path = [name.as_slice(), &path].concat();
                index::path(&path)?;
                value.flags |= SKIP;
                ensure!(
                    entries.insert(path, value).is_none(),
                    "overlapping sparse entries"
                );
            }
        } else {
            index::path(&name)?;
            if stage == 0 {
                ensure!(
                    entries.insert(name, entry).is_none(),
                    "duplicate index path"
                );
            } else {
                conflicts.entry(name).or_insert([None; 3])[usize::from(stage) - 1] = Some(entry);
            }
        }
        ensure!(
            entries.len() + conflicts.len() <= MAX_OBJECTS,
            "expanded index exceeds limit"
        );
    }
    index::validate_conflicts(&entries, &conflicts)?;
    Ok((entries, conflicts))
}
