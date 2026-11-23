//! Random-access, hash-verified reads of local packs. Index data is never trusted
//! to identify an object without checking that object's canonical hash.
use crate::{
    object::{self, Kind, MAX_OBJECT, MAX_OBJECTS, MAX_PACK, Object, Oid},
    pack,
};
use anyhow::{Context, Result, ensure};
use std::collections::HashMap;

pub struct Packed {
    format: object::Format,
    data: Vec<u8>,
    pub ids: HashMap<Oid, usize>,
    offsets: HashMap<usize, Oid>,
}
pub enum Entry {
    Object(Object),
    Delta(Oid, Vec<u8>),
}
fn u32_at(bytes: &[u8], pos: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(
        bytes
            .get(pos..pos + 4)
            .context("truncated pack index")?
            .try_into()?,
    ))
}
impl Packed {
    pub fn new_for(data: Vec<u8>, index: &[u8], format: object::Format) -> Result<Self> {
        let hash_len = format.bytes();
        ensure!(
            (12 + hash_len..=MAX_PACK).contains(&data.len()) && &data[..4] == b"PACK",
            "invalid local pack"
        );
        ensure!(
            matches!(u32_at(&data, 4)?, 2 | 3),
            "unsupported pack version"
        );
        let count = u32_at(&data, 8)? as usize;
        ensure!(count <= MAX_OBJECTS, "pack object count exceeds limit");
        let trailer = data.len() - hash_len;
        ensure!(
            object::hash_for(format, &[&data[..trailer]])?.0 == data[trailer..],
            "pack checksum mismatch"
        );
        ensure!(
            index.len() >= 1032 + 2 * hash_len
                && &index[..4] == b"\xfftOc"
                && u32_at(index, 4)? == 2,
            "requires a v2 Git pack index"
        );
        ensure!(
            object::hash_for(format, &[&index[..index.len() - hash_len]])?.0
                == index[index.len() - hash_len..],
            "pack index checksum mismatch"
        );
        ensure!(
            index[index.len() - 2 * hash_len..index.len() - hash_len] == data[trailer..],
            "pack index identifies a different pack"
        );
        ensure!(
            u32_at(index, 1028)? as usize == count,
            "index object count mismatch"
        );
        let names = 1032;
        let offsets_at = names + count * (hash_len + 4);
        let large_at = offsets_at + count * 4;
        ensure!(
            large_at + 2 * hash_len <= index.len()
                && (index.len() - large_at - 2 * hash_len).is_multiple_of(8),
            "invalid pack index length"
        );
        let mut ids = HashMap::new();
        let mut offsets = HashMap::new();
        let mut previous = None;
        let mut buckets = [0u32; 256];
        for i in 0..count {
            let id = Oid::from_bytes(&index[names + i * hash_len..names + (i + 1) * hash_len])?;
            ensure!(
                previous.is_none_or(|p| p < id),
                "pack index IDs are not ordered"
            );
            previous = Some(id);
            buckets[id.0[0] as usize] += 1;
            let encoded = u32_at(index, offsets_at + i * 4)?;
            let offset = if encoded & 0x8000_0000 == 0 {
                encoded as usize
            } else {
                let position = large_at + (encoded & 0x7fff_ffff) as usize * 8;
                ensure!(
                    position + 8 <= index.len() - 2 * hash_len,
                    "invalid large pack offset"
                );
                usize::try_from(u64::from_be_bytes(
                    index[position..position + 8].try_into()?,
                ))?
            };
            ensure!(
                (12..trailer).contains(&offset),
                "pack offset outside object data"
            );
            ensure!(
                offsets.insert(offset, id).is_none(),
                "duplicate pack offset"
            );
            ids.insert(id, offset);
        }
        let mut cumulative = 0;
        for (i, bucket) in buckets.iter().enumerate() {
            cumulative += bucket;
            ensure!(
                u32_at(index, 8 + i * 4)? == cumulative,
                "invalid pack index fanout"
            );
        }
        Ok(Self {
            data,
            ids,
            offsets,
            format,
        })
    }
    pub fn entry(&self, offset: usize) -> Result<Entry> {
        let hash_len = self.format.bytes();
        let data = &self.data[..self.data.len() - hash_len];
        let mut pos = offset;
        let mut read = || -> Result<u8> {
            let b = *data.get(pos).context("truncated pack entry")?;
            pos += 1;
            Ok(b)
        };
        let first = read()?;
        let kind = (first >> 4) & 7;
        let mut b = first;
        let mut size = u64::from(first & 15);
        let mut shift = 4;
        while b & 128 != 0 {
            ensure!(shift < 60, "overlong pack size");
            b = read()?;
            size |= u64::from(b & 127) << shift;
            shift += 7;
        }
        let size = usize::try_from(size)?;
        ensure!(size <= MAX_OBJECT, "pack object exceeds limit");
        let base = match kind {
            6 => {
                let mut b = read()?;
                let mut distance = usize::from(b & 127);
                while b & 128 != 0 {
                    b = read()?;
                    distance = distance
                        .checked_add(1)
                        .and_then(|n| n.checked_mul(128))
                        .and_then(|n| n.checked_add(usize::from(b & 127)))
                        .context("delta offset overflow")?;
                }
                ensure!(distance > 0 && distance <= offset, "invalid delta offset");
                Some(
                    *self
                        .offsets
                        .get(&(offset - distance))
                        .context("delta does not identify an indexed object")?,
                )
            }
            7 => {
                let mut id = vec![0; hash_len];
                for byte in &mut id {
                    *byte = read()?;
                }
                Some(Oid::from_bytes(&id)?)
            }
            _ => {
                Kind::from_number(kind)?;
                None
            }
        };
        let (body, _) = pack::inflate(&data[pos..], size)?;
        Ok(match base {
            Some(id) => Entry::Delta(id, body),
            None => Entry::Object(Object {
                kind: Kind::from_number(kind)?,
                data: body,
            }),
        })
    }
}
