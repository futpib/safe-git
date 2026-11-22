//! Deterministic Git-compatible ustar/PAX output. Attributes and archive filters are absent.
use crate::{
    index,
    object::{Kind, MAX_OBJECTS, MAX_TOTAL, Oid, ReadObjects},
    repo::Store,
    revision,
};
use anyhow::{Context, Result, ensure};
fn octal(field: &mut [u8], value: u64) -> Result<()> {
    let digits = format!("{:0width$o}", value, width = field.len() - 1);
    ensure!(
        digits.len() < field.len(),
        "archive number exceeds ustar range"
    );
    field[..digits.len()].copy_from_slice(digits.as_bytes());
    Ok(())
}
fn pax(key: &str, value: &[u8]) -> Vec<u8> {
    let base = key.len() + value.len() + 3;
    let mut size = base + 1;
    loop {
        let next = base + size.to_string().len();
        if next == size {
            break;
        }
        size = next;
    }
    let mut bytes = format!("{size} {key}=").into_bytes();
    bytes.extend_from_slice(value);
    bytes.push(b'\n');
    bytes
}
fn header(
    name: &[u8],
    prefix: &[u8],
    mode: u32,
    kind: u8,
    size: usize,
    mtime: u64,
    link: &[u8],
) -> Result<[u8; 512]> {
    ensure!(
        name.len() <= 100 && prefix.len() <= 155 && link.len() <= 100,
        "invalid ustar field"
    );
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name);
    h[345..345 + prefix.len()].copy_from_slice(prefix);
    h[157..157 + link.len()].copy_from_slice(link);
    octal(&mut h[100..108], u64::from(mode))?;
    octal(&mut h[108..116], 0)?;
    octal(&mut h[116..124], 0)?;
    octal(&mut h[124..136], size as u64)?;
    octal(&mut h[136..148], mtime)?;
    h[156] = kind;
    h[257..265].copy_from_slice(b"ustar\x0000");
    h[265..269].copy_from_slice(b"root");
    h[297..301].copy_from_slice(b"root");
    octal(&mut h[329..337], 0)?;
    octal(&mut h[337..345], 0)?;
    let sum = h.iter().map(|b| u64::from(*b)).sum::<u64>() + 8 * 32;
    octal(&mut h[148..156], sum)?;
    Ok(h)
}
fn append(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    ensure!(
        out.len() + bytes.len() + 512 <= MAX_TOTAL,
        "archive exceeds byte limit"
    );
    out.extend_from_slice(bytes);
    out.resize(out.len().div_ceil(512) * 512, 0);
    Ok(())
}
fn entry(
    out: &mut Vec<u8>,
    path: &[u8],
    oid: Oid,
    mode: u32,
    data: &[u8],
    mtime: u64,
) -> Result<()> {
    let mut extra = Vec::new();
    let mut name = path.to_vec();
    let mut prefix = Vec::new();
    if name.len() > 100 {
        let split = (0..name.len().min(155))
            .rev()
            .find(|i| name[*i] == b'/' && *i + 1 < name.len());
        if let Some(i) = split.filter(|i| name.len() - i - 1 <= 100) {
            prefix = name[..i].to_vec();
            name = name[i + 1..].to_vec();
        } else {
            extra.extend(pax("path", path));
            name = format!("{oid}.data").into_bytes();
        }
    }
    let (kind, mode, size, mut link) = match mode {
        0o40000 | 0o160000 => (b'5', 0o775, 0, Vec::new()),
        0o120000 => (b'2', 0o777, 0, data.to_vec()),
        0o100644 => (b'0', 0o664, data.len(), Vec::new()),
        0o100755 => (b'0', 0o775, data.len(), Vec::new()),
        _ => anyhow::bail!("invalid archive mode"),
    };
    if link.len() > 100 {
        extra.extend(pax("linkpath", &link));
        link = format!("see {oid}.paxheader").into_bytes();
    }
    if !extra.is_empty() {
        append(
            out,
            &header(
                format!("{oid}.paxheader").as_bytes(),
                b"",
                0o666,
                b'x',
                extra.len(),
                mtime,
                b"",
            )?,
        )?;
        append(out, &extra)?;
    }
    append(
        out,
        &header(&name, &prefix, mode, kind, size, mtime, &link)?,
    )?;
    if kind == b'0' {
        append(out, data)?;
    }
    Ok(())
}
pub fn tar(store: &mut Store<'_>, id: Oid, prefix: &str) -> Result<Vec<u8>> {
    if !prefix.is_empty() {
        index::path(prefix.trim_end_matches('/').as_bytes())?;
        ensure!(prefix.ends_with('/'), "archive prefix must end with /");
    }
    let id = revision::peel(store, id)?;
    let object = store.get(id)?;
    let mut out = Vec::new();
    let (tree, mtime) = if object.kind == Kind::Commit {
        let line = object
            .data
            .split(|b| *b == b'\n')
            .find_map(|l| l.strip_prefix(b"committer "))
            .context("missing commit timestamp")?;
        let time = std::str::from_utf8(line)?
            .rsplit(' ')
            .nth(1)
            .context("missing commit timestamp")?
            .parse::<u64>()?;
        let mut global = pax("comment", id.to_string().as_bytes());
        const MAX_TIME: u64 = 0o77777777777;
        if time > MAX_TIME {
            global.extend(pax("mtime", time.to_string().as_bytes()));
        }
        append(
            &mut out,
            &header(
                b"pax_global_header",
                b"",
                0o666,
                b'g',
                global.len(),
                time.min(MAX_TIME),
                b"",
            )?,
        )?;
        append(&mut out, &global)?;
        (object.commit()?.0, time.min(MAX_TIME))
    } else {
        ensure!(
            object.kind == Kind::Tree,
            "archive requires a commit or tree"
        );
        (
            id,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs(),
        )
    };
    if !prefix.is_empty() {
        entry(&mut out, prefix.as_bytes(), tree, 0o40000, b"", mtime)?;
    }
    let mut count = 0;
    let children = children(store, tree, prefix.as_bytes(), &mut count)?;
    emit(store, &mut out, children, mtime, 0, &mut count)?;
    out.resize((out.len() + 1024).div_ceil(10240) * 10240, 0);
    ensure!(out.len() <= MAX_TOTAL, "archive exceeds limit");
    Ok(out)
}
fn emit(
    store: &mut Store<'_>,
    out: &mut Vec<u8>,
    entries: Vec<(Vec<u8>, Oid, u32)>,
    mtime: u64,
    depth: usize,
    count: &mut usize,
) -> Result<()> {
    ensure!(depth <= 128, "archive tree depth exceeded");
    for (path, id, mode) in entries {
        let data = if mode == 0o40000 || mode == 0o160000 {
            Vec::new()
        } else {
            let obj = store.get(id)?;
            ensure!(obj.kind == Kind::Blob, "archive requires blob");
            obj.data.clone()
        };
        if mode == 0o120000 {
            crate::checkout::symlink_target(&path, &data)?;
        }
        entry(out, &path, id, mode, &data, mtime)?;
        if mode == 0o40000 {
            let children = children(store, id, &path, count)?;
            emit(store, out, children, mtime, depth + 1, count)?;
        }
    }
    Ok(())
}

fn children(
    store: &mut Store<'_>,
    id: Oid,
    path: &[u8],
    count: &mut usize,
) -> Result<Vec<(Vec<u8>, Oid, u32)>> {
    let obj = store.get(id)?;
    ensure!(obj.kind == Kind::Tree, "invalid archive tree");
    let mut bytes = obj.data.as_slice();
    let mut children = Vec::new();
    let mut names = std::collections::HashSet::new();
    while !bytes.is_empty() {
        *count += 1;
        ensure!(*count <= MAX_OBJECTS, "archive entry limit exceeded");
        let space = bytes
            .iter()
            .position(|b| *b == b' ')
            .context("invalid tree")?;
        let mode = u32::from_str_radix(std::str::from_utf8(&bytes[..space])?, 8)?;
        let rest = &bytes[space + 1..];
        let nul = rest.iter().position(|b| *b == 0).context("invalid tree")?;
        ensure!(rest.len() >= nul + 21, "truncated tree");
        let name = &rest[..nul];
        ensure!(
            !name.contains(&b'/') && names.insert(name.to_vec()),
            "invalid tree name"
        );
        let mut full = path.to_vec();
        full.extend(name);
        index::path(&full)?;
        let id = Oid(rest[nul + 1..nul + 21].try_into()?);
        if mode == 0o40000 || mode == 0o160000 {
            full.push(b'/');
        } else {
            index::mode(mode)?;
        }
        children.push((full, id, mode));
        bytes = &rest[nul + 21..];
    }
    children.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(children)
}
