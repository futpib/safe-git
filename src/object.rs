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

#[derive(
    Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, serde::Serialize, serde::Deserialize,
)]
pub struct Oid(pub [u8; 20]);
impl Oid {
    pub const ZERO: Self = Self([0; 20]);
    pub fn parse(s: &[u8]) -> Result<Self> {
        ensure!(
            s.len() == 40 && s.iter().all(u8::is_ascii_hexdigit),
            "expected a SHA-1 object ID"
        );
        let mut bytes = [0; 20];
        hex::decode_to_slice(s, &mut bytes)?;
        Ok(Self(bytes))
    }
}
impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

pub fn hash(parts: &[&[u8]]) -> Result<Oid> {
    let mut h = Sha1::new();
    for part in parts {
        Digest::update(&mut h, part);
    }
    let result = h.try_finalize();
    ensure!(!result.has_collision(), "SHA-1 collision detected");
    let mut bytes = [0; 20];
    bytes.copy_from_slice(result.hash());
    Ok(Oid(bytes))
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
    pub fn oid(&self) -> Result<Oid> {
        let header = format!("{} {}\0", self.kind.name(), self.data.len());
        hash(&[header.as_bytes(), &self.data])
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
                parents.push(Oid::parse(parent)?);
            }
        }
        Ok((tree, parents))
    }
    pub fn children(&self) -> Result<Vec<(Oid, Kind)>> {
        match self.kind {
            Kind::Commit => {
                let (tree, parents) = self.commit()?;
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
                    ensure!(rest.len() >= nul + 21, "truncated tree ID");
                    let mut id = [0; 20];
                    id.copy_from_slice(&rest[nul + 1..nul + 21]);
                    let kind = match mode {
                        b"40000" | b"040000" => Some(Kind::Tree),
                        b"100644" | b"100755" | b"120000" => Some(Kind::Blob),
                        b"160000" => None, // Gitlinks are inert; never fetch submodules.
                        _ => bail!("unsupported tree mode"),
                    };
                    if let Some(kind) = kind {
                        out.push((Oid(id), kind));
                    }
                    data = &rest[nul + 21..];
                }
                Ok(out)
            }
            Kind::Blob => Ok(Vec::new()),
            Kind::Tag => bail!("tags are not branch tips"),
        }
    }
}

pub type Objects = HashMap<Oid, Arc<Object>>;
pub trait ReadObjects {
    fn get(&mut self, oid: Oid) -> Result<Arc<Object>>;
}

pub fn reachable(store: &mut impl ReadObjects, tip: Oid) -> Result<Vec<Oid>> {
    reachable_since(store, tip, &HashSet::new())
}
pub fn reachable_since(
    store: &mut impl ReadObjects,
    tip: Oid,
    boundary: &HashSet<Oid>,
) -> Result<Vec<Oid>> {
    let mut seen = HashSet::new();
    let mut pending = vec![(tip, Kind::Commit)];
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
        let children = obj.children()?;
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
        let (_, parents) = store.get(id)?.commit()?;
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
        let (_, parents) = store.get(id)?.commit()?;
        ensure!(
            pending.len() + parents.len() <= MAX_OBJECTS * 4,
            "ancestry fanout exceeds limit"
        );
        pending.extend(parents);
    }
    Ok(false)
}
