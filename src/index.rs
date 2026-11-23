use crate::object::{self, Kind, MAX_OBJECTS, Oid, ReadObjects};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub mode: u32,
    pub oid: Oid,
    #[serde(default)]
    pub flags: u16,
}
pub const INTENT: u16 = 0x2000;
pub const SKIP: u16 = 0x4000;
// Flags describe the index, not the content of a tree or working file.
impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.mode == other.mode && self.oid == other.oid
    }
}
impl Eq for Entry {}
pub type Entries = BTreeMap<Vec<u8>, Entry>;
pub type Conflicts = BTreeMap<Vec<u8>, [Option<Entry>; 3]>;

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
    let hash_len = store.format().bytes();
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
            ensure!(rest.len() >= nul + 1 + hash_len, "truncated tree ID");
            let id = Oid::from_bytes(&rest[nul + 1..nul + 1 + hash_len])?;
            if mode == 0o40000 {
                pending.push((full, id, depth + 1));
            } else {
                self::mode(mode)?;
                ensure!(entries.len() < MAX_OBJECTS, "too many tree entries");
                ensure!(
                    entries
                        .insert(
                            full,
                            Entry {
                                mode,
                                oid: id,
                                flags: 0
                            }
                        )
                        .is_none(),
                    "duplicate tree path"
                );
            }
            data = &rest[nul + 1 + hash_len..];
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
#[cfg(test)]
pub fn decode(bytes: &[u8]) -> Result<Entries> {
    let (entries, conflicts) = decode_full(bytes)?;
    ensure!(
        conflicts.is_empty(),
        "unmerged index entries; resolve conflicts and add the files first"
    );
    Ok(entries)
}
#[cfg(test)]
pub fn decode_full(bytes: &[u8]) -> Result<(Entries, Conflicts)> {
    crate::extended_index::decode(None, bytes)
}
pub fn decode_repo(repo: &crate::repo::Repo, bytes: &[u8]) -> Result<(Entries, Conflicts)> {
    crate::extended_index::decode(Some(repo), bytes)
}
pub(crate) type Records = Vec<(Vec<u8>, u16, Entry)>;
pub(crate) struct RawIndex {
    pub records: Records,
    pub link: Option<Vec<u8>>,
    pub sparse: bool,
}
pub(crate) fn decode_raw(bytes: &[u8], format: object::Format) -> Result<RawIndex> {
    let hash_len = format.bytes();
    ensure!(
        bytes.len() >= 12 + hash_len && &bytes[..4] == b"DIRC",
        "invalid Git index"
    );
    let end = bytes.len() - hash_len;
    ensure!(
        object::hash_for(format, &[&bytes[..end]])?.0 == bytes[end..],
        "index checksum mismatch"
    );
    let version = u32::from_be_bytes(bytes[4..8].try_into()?);
    ensure!((2..=4).contains(&version), "unsupported index version");
    let count = u32::from_be_bytes(bytes[8..12].try_into()?) as usize;
    ensure!(count <= MAX_OBJECTS, "index entry limit exceeded");
    let mut pos = 12;
    let mut records = Vec::new();
    let mut previous = Vec::<u8>::new();

    for _ in 0..count {
        let start = pos;
        ensure!(pos + 42 + hash_len <= end, "truncated index entry");
        let mode = u32::from_be_bytes(bytes[pos + 24..pos + 28].try_into()?);
        if mode != 0o40000 {
            self::mode(mode)?;
        }
        let oid = Oid::from_bytes(&bytes[pos + 40..pos + 40 + hash_len])?;
        let flags = u16::from_be_bytes(bytes[pos + 40 + hash_len..pos + 42 + hash_len].try_into()?);
        pos += 42 + hash_len;
        let stage = (flags >> 12) & 3;
        ensure!(
            flags & 0x8000 == 0,
            "assume-unchanged entries are unsupported"
        );
        let extended = if flags & 0x4000 != 0 {
            ensure!(
                version >= 3 && pos + 2 <= end,
                "invalid extended index entry"
            );
            let value = u16::from_be_bytes(bytes[pos..pos + 2].try_into()?);
            pos += 2;
            ensure!(
                value & !(INTENT | SKIP) == 0 && stage == 0,
                "unsupported extended index flags"
            );
            ensure!(
                value != (INTENT | SKIP),
                "incompatible extended index flags"
            );
            value
        } else {
            0
        };
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
        if !name.is_empty() {
            path(name.strip_suffix(b"/").unwrap_or(&name))?;
        }
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
        previous = name.clone();
        records.push((
            name,
            stage,
            Entry {
                mode,
                oid,
                flags: extended,
            },
        ));
    }
    let mut link = None;
    let mut sparse = false;
    while pos < end {
        ensure!(pos + 8 <= end, "truncated index extension");
        let signature = &bytes[pos..pos + 4];
        ensure!(
            signature[0].is_ascii_uppercase() || signature == b"link" || signature == b"sdir",
            "unknown required index extension"
        );
        let size = u32::from_be_bytes(bytes[pos + 4..pos + 8].try_into()?) as usize;
        pos += 8;
        ensure!(size <= end - pos, "index extension exceeds input");
        if signature == b"link" {
            ensure!(link.is_none(), "duplicate split index extension");
            link = Some(bytes[pos..pos + size].to_vec());
        }
        if signature == b"sdir" {
            ensure!(!sparse && size == 0, "invalid sparse index extension");
            sparse = true;
        }
        pos += size;
    }
    Ok(RawIndex {
        records,
        link,
        sparse,
    })
}
#[cfg(test)]
pub fn encode(entries: &Entries) -> Result<Vec<u8>> {
    encode_for(entries, object::Format::Sha1)
}
pub fn encode_for(entries: &Entries, format: object::Format) -> Result<Vec<u8>> {
    encode_full_for(entries, &Conflicts::new(), format)
}
pub(crate) fn validate_conflicts(entries: &Entries, conflicts: &Conflicts) -> Result<()> {
    let mut paths = entries.clone();
    for (path, stages) in conflicts {
        ensure!(
            !entries.contains_key(path),
            "conflict and stage-zero entry at same path"
        );
        let entry = stages.iter().flatten().next().context("empty conflict")?;
        paths.insert(path.clone(), *entry);
    }
    no_prefix_conflicts(&paths)
}
pub fn encode_full_for(
    entries: &Entries,
    conflicts: &Conflicts,
    format: object::Format,
) -> Result<Vec<u8>> {
    validate_conflicts(entries, conflicts)?;
    ensure!(entries.len() <= MAX_OBJECTS, "index entry limit exceeded");
    no_prefix_conflicts(entries)?;
    let mut out = b"DIRC\0\0\0\x02".to_vec();
    if entries.values().any(|e| e.flags != 0) {
        out[7] = 3;
    }
    let mut records = BTreeMap::new();
    for (path, entry) in entries {
        records.insert((path, 0u16), entry);
    }
    for (path, stages) in conflicts {
        ensure!(
            !entries.contains_key(path),
            "stage zero and conflict at same path"
        );
        for (i, entry) in stages.iter().enumerate() {
            if let Some(entry) = entry {
                records.insert((path, (i + 1) as u16), entry);
            }
        }
    }
    ensure!(records.len() <= MAX_OBJECTS, "index entry limit exceeded");
    out.extend_from_slice(&(records.len() as u32).to_be_bytes());
    for ((name, stage), entry) in records {
        path(name)?;
        mode(entry.mode)?;
        let start = out.len();
        // Zero stat data forces future readers to verify content rather than trust a cache.
        out.extend_from_slice(&[0; 24]);
        out.extend_from_slice(&entry.mode.to_be_bytes());
        out.extend_from_slice(&[0; 12]);
        ensure!(entry.oid.format() == format, "index object format differs");
        out.extend_from_slice(&entry.oid.0);
        ensure!(
            entry.flags & !(INTENT | SKIP) == 0
                && entry.flags != (INTENT | SKIP)
                && (stage == 0 || entry.flags == 0),
            "invalid index flags"
        );
        let extended = if entry.flags != 0 { 0x4000 } else { 0 };
        out.extend_from_slice(
            &((name.len().min(0xfff) as u16) | (stage << 12) | extended).to_be_bytes(),
        );
        if extended != 0 {
            out.extend_from_slice(&entry.flags.to_be_bytes());
        }
        out.extend_from_slice(name);
        out.push(0);
        while !(out.len() - start).is_multiple_of(8) {
            out.push(0);
        }
    }
    out.extend_from_slice(&object::hash_for(format, &[&out])?.0);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extended_flags_roundtrip_and_reserved_bits_fail_closed() {
        for flags in [INTENT, SKIP] {
            let entries = Entries::from([(
                b"file".to_vec(),
                Entry {
                    mode: 0o100644,
                    oid: Oid::sha1([1; 20]),
                    flags,
                },
            )]);
            let bytes = encode(&entries).unwrap();
            let decoded = decode(&bytes).unwrap();
            assert_eq!(decoded[b"file".as_slice()].flags, flags);
            assert_eq!(encode(&decoded).unwrap(), bytes);
            for extended in [0x8000u16, INTENT | SKIP, 1] {
                let mut bad = bytes.clone();
                bad[74..76].copy_from_slice(&extended.to_be_bytes());
                bad.truncate(bad.len() - 20);
                bad.extend_from_slice(&object::hash(&[&bad]).unwrap().0);
                assert!(decode(&bad).is_err());
            }
        }
    }
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
                oid: Oid::sha1([1; 20]),
                flags: 0,
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
