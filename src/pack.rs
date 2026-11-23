use crate::object::{
    self, Kind, MAX_OBJECT, MAX_OBJECTS, MAX_PACK, MAX_TOTAL, Object, Objects, Oid,
};
use anyhow::{Result, bail, ensure};
use flate2::{Compression, Decompress, FlushDecompress, Status, write::ZlibEncoder};
use std::{collections::HashMap, io::Write, sync::Arc};

pub fn inflate(input: &[u8], size: usize) -> Result<(Vec<u8>, usize)> {
    ensure!(size <= MAX_OBJECT + 128, "inflated object exceeds limit");
    let mut output = vec![0; size + 1];
    let mut z = Decompress::new(true);
    let status = z.decompress(input, &mut output, FlushDecompress::Finish)?;
    ensure!(
        status == Status::StreamEnd,
        "truncated or oversized zlib stream"
    );
    ensure!(z.total_out() as usize == size, "object size mismatch");
    output.truncate(size);
    Ok((output, z.total_in() as usize))
}

fn byte(data: &[u8], pos: &mut usize) -> Result<u8> {
    let b = *data
        .get(*pos)
        .ok_or_else(|| anyhow::anyhow!("truncated pack/delta"))?;
    *pos += 1;
    Ok(b)
}
fn varint(data: &[u8], pos: &mut usize) -> Result<usize> {
    let mut value = 0u64;
    for shift in (0..63).step_by(7) {
        let b = byte(data, pos)?;
        value |= u64::from(b & 127) << shift;
        if b & 128 == 0 {
            return usize::try_from(value).map_err(Into::into);
        }
    }
    bail!("overlong delta integer")
}

pub fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
    let mut pos = 0;
    ensure!(
        varint(delta, &mut pos)? == base.len(),
        "delta base length mismatch"
    );
    let size = varint(delta, &mut pos)?;
    ensure!(size <= MAX_OBJECT, "delta result exceeds limit");
    let mut out = Vec::with_capacity(size);
    while pos < delta.len() {
        let op = byte(delta, &mut pos)?;
        if op & 128 != 0 {
            let mut offset = 0usize;
            let mut len = 0usize;
            for i in 0..4 {
                if op & (1 << i) != 0 {
                    offset |= usize::from(byte(delta, &mut pos)?) << (i * 8);
                }
            }
            for i in 0..3 {
                if op & (1 << (i + 4)) != 0 {
                    len |= usize::from(byte(delta, &mut pos)?) << (i * 8);
                }
            }
            if len == 0 {
                len = 0x10000;
            }
            let end = offset
                .checked_add(len)
                .ok_or_else(|| anyhow::anyhow!("delta offset overflow"))?;
            ensure!(
                end <= base.len() && len <= size.saturating_sub(out.len()),
                "delta copy out of bounds"
            );
            out.extend_from_slice(&base[offset..end]);
        } else {
            let len = usize::from(op);
            ensure!(
                len != 0 && pos + len <= delta.len() && len <= size.saturating_sub(out.len()),
                "invalid delta literal"
            );
            out.extend_from_slice(&delta[pos..pos + len]);
            pos += len;
        }
    }
    ensure!(out.len() == size, "delta result length mismatch");
    Ok(out)
}

enum Base {
    Offset(usize),
    Id(Oid),
}
struct Delta {
    offset: usize,
    base: Base,
    data: Vec<u8>,
}

#[cfg(test)]
pub fn decode(data: &[u8]) -> Result<Objects> {
    decode_for(data, object::Format::Sha1)
}
pub fn decode_for(data: &[u8], format: object::Format) -> Result<Objects> {
    let hash_len = format.bytes();
    ensure!(
        (12 + hash_len..=MAX_PACK).contains(&data.len()),
        "pack size outside limits"
    );
    ensure!(&data[..4] == b"PACK", "missing PACK signature");
    let version = u32::from_be_bytes(data[4..8].try_into()?);
    ensure!(version == 2 || version == 3, "unsupported pack version");
    let count = u32::from_be_bytes(data[8..12].try_into()?) as usize;
    ensure!(count <= MAX_OBJECTS, "pack object count exceeds limit");
    let payload_end = data.len() - hash_len;
    ensure!(
        object::hash_for(format, &[&data[..payload_end]])?.0 == data[payload_end..],
        "pack checksum mismatch"
    );
    let data = &data[..payload_end];
    let mut pos = 12;
    let mut objects = Objects::new();
    let mut offsets = HashMap::new();
    let mut depths = HashMap::new();
    let mut pending = Vec::new();
    let mut inflated = 0usize;
    for _ in 0..count {
        let offset = pos;
        let first = byte(data, &mut pos)?;
        let kind = (first >> 4) & 7;
        let mut size = u64::from(first & 15);
        let mut b = first;
        let mut shift = 4;
        while b & 128 != 0 {
            ensure!(shift < 60, "overlong pack object size");
            b = byte(data, &mut pos)?;
            size |= u64::from(b & 127) << shift;
            shift += 7;
        }
        let size = usize::try_from(size)?;
        ensure!(size <= MAX_OBJECT, "pack object exceeds limit");
        let base = match kind {
            6 => {
                let mut b = byte(data, &mut pos)?;
                let mut back = usize::from(b & 127);
                while b & 128 != 0 {
                    b = byte(data, &mut pos)?;
                    back = back
                        .checked_add(1)
                        .and_then(|n| n.checked_mul(128))
                        .and_then(|n| n.checked_add(usize::from(b & 127)))
                        .ok_or_else(|| anyhow::anyhow!("delta offset overflow"))?;
                }
                ensure!(back > 0 && back <= offset, "invalid delta base offset");
                Some(Base::Offset(offset - back))
            }
            7 => {
                ensure!(pos + hash_len <= data.len(), "truncated delta base ID");
                let id = Oid::from_bytes(&data[pos..pos + hash_len])?;
                pos += hash_len;
                Some(Base::Id(id))
            }
            _ => {
                Kind::from_number(kind)?;
                None
            }
        };
        inflated = inflated
            .checked_add(size)
            .ok_or_else(|| anyhow::anyhow!("size overflow"))?;
        ensure!(inflated <= MAX_TOTAL, "pack inflated bytes exceed limit");
        let (body, used) = inflate(&data[pos..], size)?;
        pos += used;
        if let Some(base) = base {
            pending.push(Delta {
                offset,
                base,
                data: body,
            });
        } else {
            let object = Arc::new(Object {
                kind: Kind::from_number(kind)?,
                data: body,
            });
            let id = object.oid_for(format)?;
            offsets.insert(offset, id);
            depths.insert(id, 0usize);
            objects.insert(id, object);
        }
    }
    ensure!(pos == data.len(), "trailing bytes before pack checksum");
    for _ in 0..64 {
        if pending.is_empty() {
            return Ok(objects);
        }
        let mut next = Vec::new();
        let mut progress = false;
        for delta in pending {
            let id = match delta.base {
                Base::Offset(n) => offsets.get(&n).copied(),
                Base::Id(id) => Some(id),
            };
            let base = id.and_then(|id| objects.get(&id).map(|obj| (id, Arc::clone(obj))));
            if let Some((id, base)) = base {
                let depth = depths[&id] + 1;
                ensure!(depth <= 64, "delta chain exceeds limit");
                let body = apply_delta(&base.data, &delta.data)?;
                inflated = inflated
                    .checked_add(body.len())
                    .ok_or_else(|| anyhow::anyhow!("size overflow"))?;
                ensure!(inflated <= MAX_TOTAL, "resolved pack bytes exceed limit");
                let object = Arc::new(Object {
                    kind: base.kind,
                    data: body,
                });
                let id = object.oid_for(format)?;
                offsets.insert(delta.offset, id);
                depths.insert(id, depth);
                objects.insert(id, object);
                progress = true;
            } else {
                next.push(delta);
            }
        }
        ensure!(
            progress,
            "unresolved delta base (thin packs are unsupported)"
        );
        pending = next;
    }
    ensure!(pending.is_empty(), "delta resolution exceeds limit");
    Ok(objects)
}

#[cfg(test)]
pub fn encode(objects: &[Arc<Object>]) -> Result<Vec<u8>> {
    encode_for(objects, object::Format::Sha1)
}
pub fn encode_for(objects: &[Arc<Object>], format: object::Format) -> Result<Vec<u8>> {
    Ok(encode_indexed_for(objects, false, format)?.0)
}
pub fn encode_indexed_for(
    objects: &[Arc<Object>],
    indexed: bool,
    format: object::Format,
) -> Result<(Vec<u8>, Vec<u8>)> {
    ensure!(objects.len() <= MAX_OBJECTS, "too many objects to push");
    let mut pack = Vec::from(&b"PACK\0\0\0\x02"[..]);
    pack.extend_from_slice(&(objects.len() as u32).to_be_bytes());
    let mut records = Vec::new();
    for obj in objects {
        let offset = pack.len();
        let mut size = obj.data.len();
        let mut b = ((obj.kind as u8) << 4) | (size & 15) as u8;
        size >>= 4;
        if size != 0 {
            b |= 128;
        }
        pack.push(b);
        while size != 0 {
            let mut b = (size & 127) as u8;
            size >>= 7;
            if size != 0 {
                b |= 128;
            }
            pack.push(b);
        }
        let mut z = ZlibEncoder::new(Vec::new(), Compression::fast());
        z.write_all(&obj.data)?;
        pack.extend(z.finish()?);
        ensure!(
            pack.len() + format.bytes() <= MAX_PACK,
            "outgoing pack exceeds limit"
        );
        if indexed {
            let mut crc = flate2::Crc::new();
            crc.update(&pack[offset..]);
            records.push((obj.oid_for(format)?, crc.sum(), u32::try_from(offset)?));
        }
    }
    let checksum = object::hash_for(format, &[&pack])?;
    pack.extend(checksum.0);
    let mut index = Vec::new();
    if indexed {
        records.sort_by_key(|r| r.0);
        ensure!(
            records.windows(2).all(|w| w[0].0 != w[1].0),
            "duplicate pack objects"
        );
        index.extend_from_slice(b"\xfftOc\0\0\0\x02");
        let mut fanout = [0u32; 256];
        for (id, _, _) in &records {
            fanout[id.0[0] as usize] += 1;
        }
        let mut sum = 0u32;
        for count in fanout {
            sum += count;
            index.extend_from_slice(&sum.to_be_bytes());
        }
        for (id, _, _) in &records {
            index.extend_from_slice(&id.0);
        }
        for (_, crc, _) in &records {
            index.extend_from_slice(&crc.to_be_bytes());
        }
        for (_, _, offset) in &records {
            ensure!(*offset < 0x8000_0000, "pack offset exceeds limit");
            index.extend_from_slice(&offset.to_be_bytes());
        }
        index.extend_from_slice(&checksum.0);
        index.extend_from_slice(&object::hash_for(format, &[&index])?.0);
    }
    Ok((pack, index))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_pack_entries_with_valid_checksums() {
        // Exercise structural rejection beyond the outer checksum check.
        for entry in [
            vec![0x50],             // reserved type
            vec![0x60, 0],          // zero OFS distance
            vec![0x60, 127],        // OFS before pack header
            vec![0x70, 1, 2, 3],    // truncated REF base
            vec![0xb0; 12],         // overlong object size
            vec![0x31, 0x78, 0x9c], // truncated zlib
        ] {
            let mut bytes = b"PACK\0\0\0\x02\0\0\0\x01".to_vec();
            bytes.extend(entry);
            bytes.extend(object::hash(&[&bytes]).unwrap().0);
            assert!(decode(&bytes).is_err());
        }
    }
    #[test]
    fn sha256_pack_boundaries() {
        let format = object::Format::Sha256;
        let empty = encode_for(&[], format).unwrap();
        assert!(decode_for(&empty, format).unwrap().is_empty());
        assert!(decode_for(&empty, object::Format::Sha1).is_err());
        for n in 0..empty.len() {
            assert!(decode_for(&empty[..n], format).is_err());
        }
        for entry in [vec![0x70, 1, 2, 3], vec![0x60, 0], vec![0xb0; 12]] {
            let mut data = b"PACK\0\0\0\x02\0\0\0\x01".to_vec();
            data.extend(entry);
            data.extend(object::hash_for(format, &[&data]).unwrap().0);
            assert!(decode_for(&data, format).is_err());
        }
    }
    #[test]
    fn delta_boundaries() {
        assert_eq!(
            apply_delta(b"abc", &[3, 4, 0x90, 3, 1, b'd']).unwrap(),
            b"abcd"
        );
        for bad in [
            &[3, 3, 0][..],
            &[3, 3, 0x91, 2, 3],
            &[3, 3, 4, 1],
            &[4, 3],
            &[3, 0xff, 0xff],
        ] {
            assert!(apply_delta(b"abc", bad).is_err());
        }
    }
    #[test]
    fn pack_roundtrip_and_corruption() {
        let obj = Arc::new(Object {
            kind: Kind::Blob,
            data: b"data".to_vec(),
        });
        let id = obj.oid().unwrap();
        let mut bytes = encode(&[obj]).unwrap();
        assert_eq!(decode(&bytes).unwrap()[&id].data, b"data");
        bytes[15] ^= 1;
        assert!(decode(&bytes).is_err());
        for n in 0..bytes.len() {
            assert!(decode(&bytes[..n]).is_err());
        }
    }
}
