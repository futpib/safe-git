use anyhow::{Result, bail, ensure};
use sha1_checked::{Digest, Sha1};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
};

pub const MAX_OBJECT: usize = 64 * 1024 * 1024;
pub const MAX_TOTAL: usize = 512 * 1024 * 1024;
pub const MAX_OBJECTS: usize = 200_000;
pub const MAX_PACK: usize = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub enum Format {
    #[default]
    Sha1,
    Sha256,
}
impl Format {
    pub fn bytes(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
        }
    }
    pub fn hex_len(self) -> usize {
        self.bytes() * 2
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "sha1" => Ok(Self::Sha1),
            "sha256" => Ok(Self::Sha256),
            _ => bail!("unsupported object format"),
        }
    }
    pub fn empty_tree(self) -> Result<Oid> {
        hash_for(self, &[b"tree 0\0"])
    }
    pub fn zero(self) -> Oid {
        match self {
            Self::Sha1 => Oid::ZERO,
            Self::Sha256 => Oid(HashBytes::Sha256([0; 32])),
        }
    }
}
#[derive(
    Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, serde::Serialize, serde::Deserialize,
)]
#[serde(untagged)]
pub enum HashBytes {
    Sha1([u8; 20]),
    Sha256([u8; 32]),
}
impl AsRef<[u8]> for HashBytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Sha1(v) => v,
            Self::Sha256(v) => v,
        }
    }
}
impl std::ops::Deref for HashBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_ref()
    }
}
impl IntoIterator for HashBytes {
    type Item = u8;
    type IntoIter = std::vec::IntoIter<u8>;
    fn into_iter(self) -> Self::IntoIter {
        self.as_ref().to_vec().into_iter()
    }
}
impl PartialEq<[u8]> for HashBytes {
    fn eq(&self, other: &[u8]) -> bool {
        self.as_ref() == other
    }
}
impl PartialEq<HashBytes> for [u8] {
    fn eq(&self, other: &HashBytes) -> bool {
        self == other.as_ref()
    }
}
#[derive(
    Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, serde::Serialize, serde::Deserialize,
)]
pub struct Oid(pub HashBytes);
impl Oid {
    pub const ZERO: Self = Self(HashBytes::Sha1([0; 20]));
    #[cfg(test)]
    pub const fn sha1(bytes: [u8; 20]) -> Self {
        Self(HashBytes::Sha1(bytes))
    }
    pub fn format(self) -> Format {
        match self.0 {
            HashBytes::Sha1(_) => Format::Sha1,
            HashBytes::Sha256(_) => Format::Sha256,
        }
    }
    pub fn is_zero(self) -> bool {
        self.0.iter().all(|b| *b == 0)
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        Ok(Self(match bytes.len() {
            20 => HashBytes::Sha1(bytes.try_into()?),
            32 => HashBytes::Sha256(bytes.try_into()?),
            _ => bail!("invalid object ID length"),
        }))
    }
    pub fn parse(s: &[u8]) -> Result<Self> {
        ensure!(
            matches!(s.len(), 40 | 64) && s.iter().all(u8::is_ascii_hexdigit),
            "expected a SHA-1 or SHA-256 object ID"
        );
        Self::from_bytes(&hex::decode(s)?)
    }
}
impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}
pub fn hash(parts: &[&[u8]]) -> Result<Oid> {
    hash_for(Format::Sha1, parts)
}
pub fn hash_for(format: Format, parts: &[&[u8]]) -> Result<Oid> {
    if format == Format::Sha256 {
        use sha2::Digest;
        let mut hash = sha2::Sha256::new();
        for part in parts {
            hash.update(part);
        }
        return Oid::from_bytes(&hash.finalize());
    }
    let mut hash = Sha1::new();
    for part in parts {
        Digest::update(&mut hash, part);
    }
    let result = hash.try_finalize();
    ensure!(!result.has_collision(), "SHA-1 collision detected");
    Oid::from_bytes(result.hash())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Commit = 1,
    Tree = 2,
    Blob = 3,
    Tag = 4,
}
impl Kind {
    pub fn from_number(n: u8) -> Result<Self> {
        match n {
            1 => Ok(Self::Commit),
            2 => Ok(Self::Tree),
            3 => Ok(Self::Blob),
            4 => Ok(Self::Tag),
            _ => bail!("invalid object type"),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Tree => "tree",
            Self::Blob => "blob",
            Self::Tag => "tag",
        }
    }
    pub fn parse(s: &[u8]) -> Result<Self> {
        match s {
            b"commit" => Ok(Self::Commit),
            b"tree" => Ok(Self::Tree),
            b"blob" => Ok(Self::Blob),
            b"tag" => Ok(Self::Tag),
            _ => bail!("invalid object type"),
        }
    }
}
#[derive(Debug)]
pub struct Object {
    pub kind: Kind,
    pub data: Vec<u8>,
}
impl Object {
    #[cfg(test)]
    pub fn oid(&self) -> Result<Oid> {
        self.oid_for(Format::Sha1)
    }
    pub fn oid_for(&self, format: Format) -> Result<Oid> {
        let header = format!("{} {}\0", self.kind.name(), self.data.len());
        hash_for(format, &[header.as_bytes(), &self.data])
    }
    pub fn commit(&self) -> Result<(Oid, Vec<Oid>)> {
        ensure!(
            self.kind == Kind::Commit,
            "branch/parent must point to a commit"
        );
        let end = self
            .data
            .windows(2)
            .position(|x| x == b"\n\n")
            .ok_or_else(|| anyhow::anyhow!("commit has no header terminator"))?;
        ensure!(end <= 1024 * 1024, "commit header exceeds limit");
        let mut lines = self.data[..end].split(|c| *c == b'\n');
        let first = lines.next().unwrap_or_default();
        let tree = Oid::parse(
            first
                .strip_prefix(b"tree ")
                .ok_or_else(|| anyhow::anyhow!("commit must begin with a tree"))?,
        )?;
        let mut parents = Vec::new();
        for line in lines {
            ensure!(!line.starts_with(b"tree "), "duplicate tree header");
            if let Some(parent) = line.strip_prefix(b"parent ") {
                ensure!(parents.len() < 4096, "too many commit parents");
                let parent = Oid::parse(parent)?;
                ensure!(
                    parent.format() == tree.format(),
                    "mixed commit object formats"
                );
                parents.push(parent);
            }
        }
        Ok((tree, parents))
    }
    pub fn children_for(&self, format: Format) -> Result<Vec<(Oid, Kind)>> {
        let hash_len = format.bytes();
        match self.kind {
            Kind::Commit => {
                let (tree, parents) = self.commit()?;
                ensure!(tree.format() == format, "commit object format mismatch");
                let mut out = vec![(tree, Kind::Tree)];
                out.extend(parents.into_iter().map(|p| (p, Kind::Commit)));
                Ok(out)
            }
            Kind::Tree => {
                let mut out = Vec::new();
                let mut data = self.data.as_slice();
                while !data.is_empty() {
                    let space = data
                        .iter()
                        .position(|c| *c == b' ')
                        .ok_or_else(|| anyhow::anyhow!("invalid tree mode"))?;
                    let mode = &data[..space];
                    let rest = &data[space + 1..];
                    let nul = rest
                        .iter()
                        .position(|c| *c == 0)
                        .ok_or_else(|| anyhow::anyhow!("invalid tree name"))?;
                    let name = &rest[..nul];
                    ensure!(
                        !name.is_empty() && !name.contains(&b'/') && name != b"." && name != b"..",
                        "invalid tree entry name"
                    );
                    ensure!(rest.len() >= nul + 1 + hash_len, "truncated tree ID");
                    let id = Oid::from_bytes(&rest[nul + 1..nul + 1 + hash_len])?;
                    let kind = match mode {
                        b"40000" | b"040000" => Some(Kind::Tree),
                        b"100644" | b"100755" | b"120000" => Some(Kind::Blob),
                        b"160000" => None, // Gitlinks are inert; never fetch submodules.
                        _ => bail!("unsupported tree mode"),
                    };
                    if let Some(kind) = kind {
                        out.push((id, kind));
                    }
                    data = &rest[nul + 1 + hash_len..];
                }
                Ok(out)
            }
            Kind::Blob => Ok(Vec::new()),
            Kind::Tag => {
                let end = self
                    .data
                    .windows(2)
                    .position(|w| w == b"\n\n")
                    .ok_or_else(|| anyhow::anyhow!("tag has no header terminator"))?;
                ensure!(
                    end <= 1024 * 1024 && !self.data[..end].contains(&0),
                    "invalid tag header"
                );
                let mut lines = self.data[..end].split(|b| *b == b'\n');
                let id = Oid::parse(
                    lines
                        .next()
                        .unwrap_or_default()
                        .strip_prefix(b"object ")
                        .ok_or_else(|| anyhow::anyhow!("invalid tag target"))?,
                )?;
                let kind = Kind::parse(
                    lines
                        .next()
                        .unwrap_or_default()
                        .strip_prefix(b"type ")
                        .ok_or_else(|| anyhow::anyhow!("invalid tag target type"))?,
                )?;
                ensure!(
                    lines
                        .next()
                        .and_then(|l| l.strip_prefix(b"tag "))
                        .is_some_and(|name| !name.is_empty()),
                    "tag has no name"
                );
                ensure!(id.format() == format, "tag object format mismatch");
                Ok(vec![(id, kind)])
            }
        }
    }
}

pub type Objects = HashMap<Oid, Arc<Object>>;
pub trait ReadObjects {
    fn format(&self) -> Format;
    fn get(&mut self, oid: Oid) -> Result<Arc<Object>>;
    fn parents(&mut self, oid: Oid) -> Result<Vec<Oid>> {
        Ok(self.get(oid)?.commit()?.1)
    }
    fn shallow(&self, _oid: Oid) -> bool {
        false
    }
}

pub fn reachable(store: &mut impl ReadObjects, tip: Oid) -> Result<Vec<Oid>> {
    reachable_since(store, tip, &HashSet::new())
}
pub fn reachable_since(
    store: &mut impl ReadObjects,
    tip: Oid,
    boundary: &HashSet<Oid>,
) -> Result<Vec<Oid>> {
    reachable_many_since(store, &[tip], boundary)
}
pub fn reachable_many_since(
    store: &mut impl ReadObjects,
    tips: &[Oid],
    boundary: &HashSet<Oid>,
) -> Result<Vec<Oid>> {
    ensure!(tips.len() <= MAX_OBJECTS, "too many object roots");
    let mut seen = HashSet::new();
    let mut pending = Vec::new();
    for tip in tips.iter().copied().collect::<HashSet<_>>() {
        pending.push((tip, store.get(tip)?.kind));
    }
    let mut bytes = 0usize;
    while let Some((oid, expected)) = pending.pop() {
        if expected == Kind::Commit && boundary.contains(&oid) {
            continue;
        }
        let obj = store.get(oid)?;
        ensure!(obj.kind == expected, "object {oid} has unexpected type");
        if !seen.insert(oid) {
            continue;
        }
        ensure!(seen.len() <= MAX_OBJECTS, "object count exceeds limit");
        bytes = bytes
            .checked_add(obj.data.len())
            .ok_or_else(|| anyhow::anyhow!("size overflow"))?;
        ensure!(bytes <= MAX_TOTAL, "reachable objects exceed byte limit");
        let mut children = obj.children_for(oid.format())?;
        if obj.kind == Kind::Commit && store.shallow(oid) {
            children.retain(|(_, kind)| *kind != Kind::Commit);
        }
        ensure!(
            pending.len() + children.len() <= MAX_OBJECTS * 4,
            "graph fanout exceeds limit"
        );
        pending.extend(children);
    }
    let mut ids: Vec<_> = seen.into_iter().collect();
    ids.sort_unstable();
    Ok(ids)
}

pub fn commits(store: &mut impl ReadObjects, tips: &[Oid]) -> Result<HashSet<Oid>> {
    let mut seen = HashSet::new();
    let mut pending = tips.to_vec();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        ensure!(seen.len() <= MAX_OBJECTS, "commit history exceeds limit");
        let parents = store.parents(id)?;
        ensure!(
            pending.len() + parents.len() <= MAX_OBJECTS * 4,
            "commit fanout exceeds limit"
        );
        pending.extend(parents);
    }
    Ok(seen)
}

pub fn ancestor(store: &mut impl ReadObjects, old: Oid, new: Oid) -> Result<bool> {
    let mut pending = vec![new];
    let mut seen = HashSet::new();
    while let Some(id) = pending.pop() {
        if id == old {
            return Ok(true);
        }
        if !seen.insert(id) {
            continue;
        }
        ensure!(
            seen.len() <= MAX_OBJECTS,
            "ancestry traversal exceeds limit"
        );
        let parents = store.parents(id)?;
        ensure!(
            pending.len() + parents.len() <= MAX_OBJECTS * 4,
            "ancestry fanout exceeds limit"
        );
        pending.extend(parents);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn id_serialization_keeps_existing_journals_readable() {
        for format in [Format::Sha1, Format::Sha256] {
            let bytes = vec![17u8; format.bytes()];
            let id = Oid::from_bytes(&bytes).unwrap();
            let original = serde_json::to_vec(&bytes).unwrap();
            assert_eq!(serde_json::to_vec(&id).unwrap(), original);
            assert_eq!(serde_json::from_slice::<Oid>(&original).unwrap(), id);
        }
        assert!(serde_json::from_slice::<Oid>(&serde_json::to_vec(&vec![0; 21]).unwrap()).is_err());
    }
}
