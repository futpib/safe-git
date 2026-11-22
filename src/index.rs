use crate::object::{self, Kind, MAX_OBJECTS, Oid, ReadObjects};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub mode: u32,
    pub oid: Oid,
}
pub type Entries = BTreeMap<Vec<u8>, Entry>;

pub fn path(name: &[u8]) -> Result<()> {
    ensure!(
        !name.is_empty() && name.len() <= 4096,
        "invalid worktree path length"
    );
    for component in name.split(|b| *b == b'/') {
        let folded = component.to_ascii_lowercase();
        ensure!(
            !component.is_empty()
                && component.len() <= 255
                && component != b"."
                && component != b".."
                && folded != b".git"
                && folded != b"git~1"
                && !component.ends_with(b".")
                && !component.ends_with(b" ")
                && !component
                    .iter()
                    .any(|b| *b < 32 || *b == 127 || b":\\".contains(b)),
            "unsafe worktree path: {:?}",
            String::from_utf8_lossy(name)
        );
    }
    Ok(())
}
pub fn mode(mode: u32) -> Result<()> {
    ensure!(
        matches!(mode, 0o100644 | 0o100755 | 0o120000 | 0o160000),
        "unsupported index/tree entry mode"
    );
    Ok(())
}
pub fn tree(store: &mut impl ReadObjects, commit: Option<Oid>) -> Result<Entries> {
    let mut entries = Entries::new();
    let Some(commit) = commit else {
        return Ok(entries);
    };
    let object = store.get(commit)?;
    let root = if object.kind == Kind::Tree {
        commit
    } else {
        object.commit()?.0
    };
    let mut pending = vec![(Vec::new(), root, 0usize)];
    let mut visited = 0;
    while let Some((prefix, oid, depth)) = pending.pop() {
        visited += 1;
        ensure!(
            visited <= MAX_OBJECTS && depth <= 128,
            "tree traversal limit exceeded"
        );
        let object = store.get(oid)?;
        ensure!(object.kind == Kind::Tree, "expected tree object");
        let mut data = object.data.as_slice();
        let mut names = std::collections::HashSet::new();
        while !data.is_empty() {
            let space = data
                .iter()
                .position(|b| *b == b' ')
                .context("invalid tree mode")?;
            let mode = u32::from_str_radix(std::str::from_utf8(&data[..space])?, 8)?;
            let rest = &data[space + 1..];
            let nul = rest
                .iter()
                .position(|b| *b == 0)
                .context("invalid tree name")?;
            let name = &rest[..nul];
            ensure!(
                !name.contains(&b'/') && names.insert(name.to_vec()),
                "duplicate or invalid tree name"
            );
            let mut full = prefix.clone();
            if !full.is_empty() {
                full.push(b'/');
            }
            full.extend_from_slice(name);
            path(&full)?;
            ensure!(rest.len() >= nul + 21, "truncated tree ID");
            let id = Oid(rest[nul + 1..nul + 21].try_into()?);
            if mode == 0o40000 {
                pending.push((full, id, depth + 1));
            } else {
                self::mode(mode)?;
                ensure!(entries.len() < MAX_OBJECTS, "too many tree entries");
                ensure!(
                    entries.insert(full, Entry { mode, oid: id }).is_none(),
                    "duplicate tree path"
                );
            }
            data = &rest[nul + 21..];
        }
    }
    no_prefix_conflicts(&entries)?;
    Ok(entries)
}
pub fn no_prefix_conflicts(entries: &Entries) -> Result<()> {
    for name in entries.keys() {
        for (i, _) in name.iter().enumerate().filter(|(_, b)| **b == b'/') {
            ensure!(
                !entries.contains_key(&name[..i]),
                "file/directory conflict in index/tree"
            );
        }
    }
    Ok(())
}
pub fn decode(bytes: &[u8]) -> Result<Entries> {
    ensure!(
        bytes.len() >= 32 && &bytes[..4] == b"DIRC",
        "invalid Git index"
    );
    let end = bytes.len() - 20;
    ensure!(
        object::hash(&[&bytes[..end]])?.0 == bytes[end..],
        "index checksum mismatch"
    );
    let version = u32::from_be_bytes(bytes[4..8].try_into()?);
    ensure!((2..=4).contains(&version), "unsupported index version");
    let count = u32::from_be_bytes(bytes[8..12].try_into()?) as usize;
    ensure!(count <= MAX_OBJECTS, "index entry limit exceeded");
    let mut pos = 12;
    let mut entries = Entries::new();
    let mut previous = Vec::<u8>::new();
    for _ in 0..count {
        let start = pos;
        ensure!(pos + 62 <= end, "truncated index entry");
        let mode = u32::from_be_bytes(bytes[pos + 24..pos + 28].try_into()?);
        self::mode(mode)?;
        let oid = Oid(bytes[pos + 40..pos + 60].try_into()?);
        let flags = u16::from_be_bytes(bytes[pos + 60..pos + 62].try_into()?);
        pos += 62;
        ensure!(
            flags & 0x3000 == 0,
            "unmerged index entries are unsupported"
        );
        ensure!(
            flags & 0xc000 == 0,
            "assume-unchanged, sparse, and extended index entries are unsupported"
        );
        let mut name = Vec::new();
        if version == 4 {
            ensure!(pos < end, "truncated index prefix");
            let mut b = *bytes.get(pos).context("truncated index name")?;
            pos += 1;
            let mut remove = usize::from(b & 127);
            while b & 128 != 0 {
                ensure!(pos < end, "truncated index name");
                b = bytes[pos];
                pos += 1;
                remove = remove
                    .checked_add(1)
                    .and_then(|v| v.checked_mul(128))
                    .and_then(|v| v.checked_add(usize::from(b & 127)))
                    .context("index prefix overflow")?;
            }
            ensure!(remove <= previous.len(), "invalid index prefix");
            name.extend_from_slice(&previous[..previous.len() - remove]);
        }
        let nul = bytes[pos..end]
            .iter()
            .position(|b| *b == 0)
            .context("unterminated index name")?;
        name.extend_from_slice(&bytes[pos..pos + nul]);
        pos += nul + 1;
        path(&name)?;
        ensure!(
            usize::from(flags & 0xfff) == name.len().min(0xfff),
            "index name length mismatch"
        );
        if version != 4 {
            let pad = (8 - (pos - start) % 8) % 8;
            ensure!(
                pos + pad <= end && bytes[pos..pos + pad].iter().all(|b| *b == 0),
                "invalid index padding"
            );
            pos += pad;
        }
        ensure!(
            previous.is_empty() || previous < name,
            "duplicate or unordered index entries"
        );
        previous = name.clone();
        entries.insert(name, Entry { mode, oid });
    }
    while pos < end {
        ensure!(pos + 8 <= end, "truncated index extension");
        let signature = &bytes[pos..pos + 4];
        ensure!(
            signature[0].is_ascii_uppercase(),
            "required index extensions (split/sparse indexes) are unsupported"
        );
        let size = u32::from_be_bytes(bytes[pos + 4..pos + 8].try_into()?) as usize;
        pos += 8;
        ensure!(size <= end - pos, "index extension exceeds input");
        pos += size;
    }
    no_prefix_conflicts(&entries)?;
    Ok(entries)
}
pub fn encode(entries: &Entries) -> Result<Vec<u8>> {
    ensure!(entries.len() <= MAX_OBJECTS, "index entry limit exceeded");
    no_prefix_conflicts(entries)?;
    let mut out = b"DIRC\0\0\0\x02".to_vec();
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (name, entry) in entries {
        path(name)?;
        mode(entry.mode)?;
        let start = out.len();
        // Zero stat data forces future readers to verify content rather than trust a cache.
        out.extend_from_slice(&[0; 24]);
        out.extend_from_slice(&entry.mode.to_be_bytes());
        out.extend_from_slice(&[0; 12]);
        out.extend_from_slice(&entry.oid.0);
        out.extend_from_slice(&(name.len().min(0xfff) as u16).to_be_bytes());
        out.extend_from_slice(name);
        out.push(0);
        while !(out.len() - start).is_multiple_of(8) {
            out.push(0);
        }
    }
    out.extend_from_slice(&object::hash(&[&out])?.0);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_v4_with_valid_checksum() {
        let mut bytes = b"DIRC\0\0\0\x04\0\0\0\x01".to_vec();
        bytes.extend_from_slice(&[0; 62]);
        bytes[36..40].copy_from_slice(&0o100644u32.to_be_bytes());
        let checksum = object::hash(&[&bytes]).unwrap();
        bytes.extend_from_slice(&checksum.0);
        assert!(
            decode(&bytes)
                .unwrap_err()
                .to_string()
                .contains("truncated index prefix")
        );
    }
    #[test]
    fn checksum_names_and_roundtrip() {
        let mut entries = Entries::new();
        entries.insert(
            b"src/main.rs".to_vec(),
            Entry {
                mode: 0o100644,
                oid: Oid([1; 20]),
            },
        );
        let bytes = encode(&entries).unwrap();
        assert_eq!(decode(&bytes).unwrap(), entries);
        for n in 0..bytes.len() {
            assert!(decode(&bytes[..n]).is_err());
        }
        for p in [
            b"../x".as_slice(),
            b"a/.git/config",
            b".GiT/hooks/a",
            b"/tmp/x",
            b"a//x",
            b"a:stream",
            b"a.",
            b"a\\b",
        ] {
            assert!(path(p).is_err());
        }
    }
}
