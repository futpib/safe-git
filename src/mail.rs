//! Inert unified patches and mailboxes. All paths pass checkout's descriptor-relative checks.
use crate::{
    Cli,
    checkout::{self, Edit, Work},
    compat,
    index::{self, Entries, Entry},
    object::{Kind, Object, Oid, ReadObjects},
    porcelain,
    repo::{Pending, Repo, Store},
};
use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{self, Mode, OFlags};
use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Write},
};
const LIMIT: usize = 16 * 1024 * 1024;

pub fn read_input(cli: &Cli, name: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    if name == "-" {
        std::io::stdin()
            .lock()
            .take(LIMIT as u64 + 1)
            .read_to_end(&mut bytes)?;
    } else {
        let path = cli.repository.join(&cli.prefix).join(name);
        let path = std::env::current_dir()?.join(path);
        let mut parts = Vec::new();
        for part in path.components() {
            match part {
                std::path::Component::Normal(v) => parts.push(v),
                std::path::Component::ParentDir => {
                    ensure!(parts.pop().is_some(), "input path escapes root");
                }
                std::path::Component::RootDir | std::path::Component::CurDir => {}
                _ => bail!("invalid input path"),
            }
        }
        let name = parts.pop().context("input requires a file")?;
        let mut dir = File::open("/")?;
        for part in parts {
            dir = File::from(fs::openat(
                &dir,
                part,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
        }
        let file = File::from(fs::openat(
            &dir,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        ensure!(meta.is_file() && meta.nlink() == 1, "unsafe input file");
        file.take(LIMIT as u64 + 1).read_to_end(&mut bytes)?;
    }
    ensure!(bytes.len() <= LIMIT, "input exceeds byte limit");
    Ok(bytes)
}
pub fn check_whitespace(path: &[u8], old: &[u8], new: &[u8]) -> Result<(bool, String)> {
    if old.contains(&0) || new.contains(&0) {
        return Ok((false, String::new()));
    }
    compat::diff_budget(old, new)?;
    let patch = diffy::create_patch_bytes(old, new);
    let mut out = String::new();
    for h in patch.hunks() {
        let mut line = h.new_range().start();
        for item in h.lines() {
            match item {
                diffy::Line::Insert(bytes) => {
                    let data = bytes.strip_suffix(b"\n").unwrap_or(bytes);
                    let data = data.strip_suffix(b"\r").unwrap_or(data);
                    let indent = &data[..data
                        .iter()
                        .position(|b| !matches!(b, b' ' | b'\t'))
                        .unwrap_or(data.len())];
                    let mut errors = Vec::new();
                    if data.last().is_some_and(|b| matches!(b, b' ' | b'\t')) {
                        errors.push("trailing whitespace");
                    }
                    if indent.windows(2).any(|v| v == b" \t") {
                        errors.push("space before tab in indent");
                    }
                    if data.starts_with(b"<<<<<<< ")
                        || data == b"======="
                        || data.starts_with(b">>>>>>> ")
                    {
                        errors.push("leftover conflict marker");
                    }
                    if !errors.is_empty() {
                        out.push_str(&format!(
                            "{}:{line}: {}.\n+{}\n",
                            crate::output::quote(path),
                            errors.join(", "),
                            compat::text(&String::from_utf8_lossy(data))
                        ));
                    }
                    line += 1;
                }
                diffy::Line::Context(_) => line += 1,
                _ => {}
            }
        }
    }
    // Git's default blank-at-eof check applies only when new blank lines are introduced.
    let trailing = |v: &[u8]| {
        v.split_inclusive(|b| *b == b'\n')
            .rev()
            .take_while(|l| l.iter().all(u8::is_ascii_whitespace))
            .count()
    };
    if trailing(new) > trailing(old) {
        out.push_str(&format!(
            "{}:{}: new blank line at EOF.\n",
            crate::output::quote(path),
            new.split_inclusive(|b| *b == b'\n').count() - trailing(new) + 1
        ));
    }
    Ok((!out.is_empty(), out))
}
#[derive(Default, Clone)]
struct Change {
    copy: bool,
    old: Option<Vec<u8>>,
    new: Option<Vec<u8>>,
    old_mode: Option<u32>,
    new_mode: Option<u32>,
    hunks: Vec<u8>,
    binary: Vec<crate::binary_patch::Block>,
    old_oid: Option<Oid>,
    new_oid: Option<Oid>,
}
fn decode_name(input: &[u8]) -> Result<Vec<u8>> {
    let input = input.strip_suffix(b"\n").unwrap_or(input);
    if !input.starts_with(b"\"") {
        return Ok(input.split(|b| *b == b'\t').next().unwrap().to_vec());
    }
    let mut out = Vec::new();
    let mut i = 1;
    while i < input.len() {
        let c = input[i];
        i += 1;
        if c == b'"' {
            ensure!(
                input[i..].is_empty() || input[i] == b'\t',
                "invalid quoted patch path"
            );
            return Ok(out);
        }
        if c != b'\\' {
            out.push(c);
            continue;
        }
        let c = *input.get(i).context("truncated quoted path")?;
        i += 1;
        if (b'0'..=b'7').contains(&c) {
            let tail = input.get(i..i + 2).context("truncated octal path escape")?;
            ensure!(
                tail.iter().all(|b| (b'0'..=b'7').contains(b)),
                "invalid octal path escape"
            );
            let n = (c - b'0') as u16 * 64 + (tail[0] - b'0') as u16 * 8 + (tail[1] - b'0') as u16;
            ensure!(n <= 255, "invalid octal path escape");
            out.push(n as u8);
            i += 2;
        } else {
            out.push(match c {
                b'\\' => b'\\',
                b'"' => b'"',
                b't' => b'\t',
                b'n' => b'\n',
                b'r' => b'\r',
                _ => bail!("invalid path escape"),
            });
        }
    }
    bail!("unterminated quoted path")
}
fn path(input: &[u8], strip: usize) -> Result<Option<Vec<u8>>> {
    let value = decode_name(input)?;
    if value == b"/dev/null" {
        return Ok(None);
    }
    ensure!(!value.starts_with(b"/"), "absolute patch path");
    let value = value
        .split(|b| *b == b'/')
        .skip(strip)
        .collect::<Vec<_>>()
        .join(&b'/');
    index::path(&value)?;
    Ok(Some(value))
}
fn mode(line: &[u8]) -> Result<u32> {
    let n = u32::from_str_radix(std::str::from_utf8(line)?.trim(), 8)?;
    ensure!(
        matches!(n, 0o100644 | 0o100755 | 0o120000 | 0o160000),
        "unsupported patch mode"
    );
    Ok(n)
}
fn parse(bytes: &[u8], strip: usize) -> Result<Vec<Change>> {
    ensure!(bytes.len() <= LIMIT && strip <= 128, "patch exceeds limit");
    let lines: Vec<_> = bytes.split_inclusive(|b| *b == b'\n').collect();
    ensure!(lines.len() <= 200_000, "patch line budget exceeded");
    let mut changes = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !lines[i].starts_with(b"diff --git ") && !lines[i].starts_with(b"--- ") {
            i += 1;
            continue;
        }
        let mut c = Change::default();
        if let Some(header) = lines[i].strip_prefix(b"diff --git ") {
            let header = header.strip_suffix(b"\n").unwrap_or(header);
            // Hunk headers below are authoritative; this also handles mode-only patches.
            if header.starts_with(b"\"") {
                let mut end = 1;
                let mut escaped = false;
                while end < header.len() {
                    if header[end] == b'"' && !escaped {
                        break;
                    }
                    if header[end] == b'\\' {
                        escaped = !escaped;
                    } else {
                        escaped = false;
                    }
                    end += 1;
                }
                ensure!(header.get(end + 1) == Some(&b' '), "invalid diff paths");
                c.old = path(&header[..=end], strip)?;
                c.new = path(&header[end + 2..], strip)?;
            } else {
                let at = header
                    .windows(3)
                    .position(|v| v == b" b/")
                    .or_else(|| header.iter().position(|b| *b == b' '))
                    .context("invalid diff paths")?;
                c.old = path(&header[..at], strip)?;
                c.new = path(&header[at + 1..], strip)?;
            }
            i += 1;
        }
        while i < lines.len() && !lines[i].starts_with(b"diff --git ") {
            let line = lines[i];
            if let Some(v) = line.strip_prefix(b"new file mode ") {
                c.old = None;
                c.new_mode = Some(mode(v)?);
            } else if let Some(v) = line.strip_prefix(b"deleted file mode ") {
                c.new = None;
                c.old_mode = Some(mode(v)?);
            } else if let Some(v) = line.strip_prefix(b"old mode ") {
                c.old_mode = Some(mode(v)?);
            } else if let Some(v) = line.strip_prefix(b"new mode ") {
                c.new_mode = Some(mode(v)?);
            } else if let Some(v) = line.strip_prefix(b"copy from ") {
                c.copy = true;
                c.old = path(v, 0)?;
            } else if let Some(v) = line.strip_prefix(b"copy to ") {
                c.copy = true;
                c.new = path(v, 0)?;
            } else if let Some(v) = line.strip_prefix(b"rename from ") {
                c.old = path(v, 0)?;
            } else if let Some(v) = line.strip_prefix(b"rename to ") {
                c.new = path(v, 0)?;
            } else if let Some(v) = line.strip_prefix(b"index ") {
                if let Some(ids) = v.split(|b| *b == b' ' || *b == b'\n').next()
                    && matches!(ids.len(), 82 | 130)
                    && &ids[(ids.len() - 2) / 2..(ids.len() + 2) / 2] == b".."
                {
                    c.old_oid = Some(Oid::parse(&ids[..(ids.len() - 2) / 2])?);
                    c.new_oid = Some(Oid::parse(&ids[(ids.len() + 2) / 2..])?);
                }
                if let Some(v) = v.split(|b| *b == b' ').nth(1) {
                    let m = mode(v)?;
                    c.old_mode = Some(m);
                    c.new_mode = Some(m);
                }
            } else if let Some(v) = line.strip_prefix(b"--- ") {
                if !c.hunks.is_empty() {
                    break;
                }
                c.old = path(v, strip)?;
                i += 1;
                c.new = path(
                    lines
                        .get(i)
                        .and_then(|v| v.strip_prefix(b"+++ "))
                        .context("missing new patch path")?,
                    strip,
                )?;
            } else if line.starts_with(b"@@ ") {
                let header = std::str::from_utf8(line)?;
                let mut fields = header.split_whitespace();
                fields.next();
                let count = |v: &str| -> Result<usize> {
                    Ok(v.split_once(',').map(|(_, n)| n).unwrap_or("1").parse()?)
                };
                let mut old = count(fields.next().context("bad hunk header")?)?;
                let mut new = count(fields.next().context("bad hunk header")?)?;
                ensure!(old <= 200_000 && new <= 200_000, "hunk exceeds limit");
                c.hunks.extend_from_slice(line);
                i += 1;
                while old > 0 || new > 0 {
                    let l = *lines.get(i).context("truncated hunk")?;
                    match l.first() {
                        Some(b' ') => {
                            old = old.checked_sub(1).context("bad hunk counts")?;
                            new = new.checked_sub(1).context("bad hunk counts")?;
                        }
                        Some(b'-') => old = old.checked_sub(1).context("bad hunk counts")?,
                        Some(b'+') => new = new.checked_sub(1).context("bad hunk counts")?,
                        Some(b'\\') if l.starts_with(b"\\ No newline at end of file") => {}
                        _ => bail!("invalid hunk line"),
                    }
                    c.hunks.extend_from_slice(l);
                    i += 1;
                }
                if lines
                    .get(i)
                    .is_some_and(|l| l.starts_with(b"\\ No newline at end of file"))
                {
                    c.hunks.extend_from_slice(lines[i]);
                    i += 1;
                }
                continue;
            } else if line.starts_with(b"GIT binary patch") {
                i += 1;
                c.binary.push(crate::binary_patch::parse(&lines, &mut i)?);
                if lines
                    .get(i)
                    .is_some_and(|l| l.starts_with(b"literal ") || l.starts_with(b"delta "))
                {
                    c.binary.push(crate::binary_patch::parse(&lines, &mut i)?);
                }
                continue;
            } else if line.starts_with(b"Binary files") {
                bail!("patch lacks applicable content or uses unsupported copy headers");
            } else if line.starts_with(b"-- ") {
                break;
            } else if !line.starts_with(b"similarity index ")
                && !line.iter().all(u8::is_ascii_whitespace)
            {
                bail!("unsupported patch header");
            }
            i += 1;
        }
        ensure!(c.old.is_some() || c.new.is_some(), "patch has no paths");
        changes.push(c);
        ensure!(changes.len() <= 10_000, "too many patch files");
    }
    ensure!(!changes.is_empty(), "no valid patches in input");
    Ok(changes)
}
fn transform(
    repo: &Repo,
    store: &mut Store<'_>,
    before: &Entries,
    changes: &[Change],
    reverse: bool,
) -> Result<Entries> {
    let mut after = before.clone();
    let mut seen = BTreeSet::new();
    let mut total = 0;
    for original in changes {
        let mut c = original.clone();
        if reverse {
            std::mem::swap(&mut c.old, &mut c.new);
            std::mem::swap(&mut c.old_mode, &mut c.new_mode);
            std::mem::swap(&mut c.old_oid, &mut c.new_oid);
        }
        for p in c.old.iter().chain(c.new.iter()) {
            index::path(p)?;
        }
        let key = c.new.as_ref().or(c.old.as_ref()).unwrap();
        ensure!(seen.insert(key.clone()), "duplicate patch destination");
        let entry = match &c.old {
            Some(p) => Some(*after.get(p).context("patch source missing")?),
            None => None,
        };
        if let Some(expected) = c.old_mode {
            ensure!(
                entry.is_none_or(|e| e.mode == expected),
                "patch source mode differs"
            );
        }
        if let Some(p) = &c.new {
            ensure!(
                c.copy && reverse || c.old.as_ref() == Some(p) || !after.contains_key(p),
                "patch would overwrite existing file"
            );
        }
        let old = match entry {
            Some(e) if e.mode == 0o160000 => format!("Subproject commit {}\n", e.oid).into_bytes(),
            Some(e) => store.get(e.oid)?.data.clone(),
            None => Vec::new(),
        };
        if !c.binary.is_empty() {
            ensure!(
                c.old_oid.is_some() && c.new_oid.is_some(),
                "binary patch requires full object IDs"
            );
            ensure!(
                c.old_oid == Some(entry.map(|e| e.oid).unwrap_or(repo.format.zero())),
                "binary patch source differs"
            );
        }
        let data = if !c.binary.is_empty() {
            let block = c
                .binary
                .get(usize::from(reverse))
                .context("binary patch cannot be reversed")?;
            let bytes = block.apply(&old)?;
            let id = if c.new.is_none() {
                ensure!(bytes.is_empty(), "binary deletion has content");
                repo.format.zero()
            } else {
                Object {
                    kind: Kind::Blob,
                    data: bytes.clone(),
                }
                .oid_for(repo.format)?
            };
            ensure!(c.new_oid == Some(id), "binary patch result hash mismatch");
            bytes
        } else if c.hunks.is_empty() {
            old.clone()
        } else {
            let patch_bytes = [b"--- old\n+++ new\n".as_slice(), &c.hunks].concat();
            let patch = diffy::Patch::from_bytes(&patch_bytes)?;
            compat::diff_budget(&old, &c.hunks)?;
            if reverse {
                diffy::apply_bytes(&old, &patch.reverse())?
            } else {
                diffy::apply_bytes(&old, &patch)?
            }
        };
        total += data.len();
        ensure!(total <= LIMIT, "patch result exceeds byte limit");
        if c.copy && reverse {
            let source = original.old.as_ref().context("copy has no source")?;
            let e = *before.get(source).context("copy source missing")?;
            ensure!(
                store.get(e.oid)?.data == data,
                "reversed copy differs from source"
            );
            after.remove(c.old.as_ref().context("copy has no destination")?);
            continue;
        }
        if !c.copy
            && let Some(p) = &c.old
        {
            after.remove(p);
        }
        if let Some(p) = c.new {
            let m = c.new_mode.or(entry.map(|e| e.mode)).unwrap_or(0o100644);
            if m == 0o120000 {
                checkout::symlink_target(&p, &data)?;
            }
            let oid = if m == 0o160000 {
                let value = data
                    .strip_prefix(b"Subproject commit ")
                    .and_then(|s| s.strip_suffix(b"\n"))
                    .context("invalid gitlink patch")?;
                Oid::parse(value)?
            } else {
                let obj = Object {
                    kind: Kind::Blob,
                    data,
                };
                let id = obj.oid_for(repo.format)?;
                repo.write_object(&obj)?;
                id
            };
            after.insert(
                p,
                Entry {
                    flags: 0,
                    mode: m,
                    oid,
                },
            );
        } else {
            ensure!(data.is_empty(), "deletion patch leaves content");
        }
    }
    index::no_prefix_conflicts(&after)?;
    Ok(after)
}
fn install_index(repo: &Repo, before: &Entries, after: &Entries) -> Result<()> {
    let head = repo.head()?;
    let tip = repo.read_ref(&head)?;
    let _head_lock = Pending::new(repo.root.try_clone()?, "HEAD.lock".into())?;
    let _ref_lock = if head != "HEAD" {
        Some(repo.prepare_ref(&head, tip)?)
    } else {
        None
    };
    let mut lock = Pending::new(repo.root.try_clone()?, "index.lock".into())?;
    ensure!(
        repo.head()? == head && porcelain::index(repo)? == *before,
        "HEAD or index changed concurrently"
    );
    lock.file
        .write_all(&index::encode_for(after, repo.format)?)?;
    lock.commit("index")
}
fn apply_changes(
    repo: &Repo,
    store: &mut Store<'_>,
    changes: &[Change],
    cached: bool,
    indexed: bool,
    check: bool,
    reverse: bool,
) -> Result<()> {
    let staged = porcelain::index(repo)?;
    let mut tracked = staged.clone();
    if !cached {
        let work = Work::new(repo)?;
        for p in changes
            .iter()
            .flat_map(|c| c.old.iter().chain(c.new.iter()))
        {
            if tracked.get(p).is_some_and(|e| e.mode == 0o160000) {
                continue;
            }
            if let Some(e) = work.state(p)? {
                tracked.insert(p.clone(), e);
            }
        }
    }
    let before = if cached {
        staged.clone()
    } else {
        checkout::snapshot(repo, &tracked)?
    };
    if indexed && !cached {
        for p in changes
            .iter()
            .flat_map(|c| c.old.iter().chain(c.new.iter()))
        {
            ensure!(
                before.get(p) == staged.get(p),
                "index and worktree differ for patch path"
            );
        }
    }
    let after = transform(repo, store, &before, changes, reverse)?;
    if check {
        return Ok(());
    }
    if cached {
        return install_index(repo, &staged, &after);
    }
    let mut next = staged.clone();
    if indexed {
        for p in changes
            .iter()
            .flat_map(|c| c.old.iter().chain(c.new.iter()))
        {
            match after.get(p) {
                Some(e) => {
                    next.insert(p.clone(), *e);
                }
                None => {
                    next.remove(p);
                }
            }
        }
    }
    checkout::edit(
        repo,
        store,
        Edit {
            head: repo.head()?,
            tip: repo.read_ref("HEAD")?.unwrap_or(repo.format.zero()),
            before,
            after,
            index_before: staged,
            index_after: next,
            conflicts_before: Default::default(),
            conflicts_after: Default::default(),
        },
    )
}
pub fn run(
    cli: &Cli,
    repo: &Repo,
    store: &mut Store<'_>,
    command: &str,
    args: &[String],
) -> Result<()> {
    match command {
        "format-patch" => format_patch(cli, repo, store, args),
        "am" => am(cli, repo, store, args),
        _ => apply(cli, repo, store, args),
    }
}
fn apply(cli: &Cli, repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    let (mut cached, mut indexed, mut check, mut reverse) = (false, false, false, false);
    let mut strip = 1;
    let mut names = Vec::new();
    let mut end = false;
    for arg in args {
        if end {
            names.push(arg.as_str());
            continue;
        }
        match arg.as_str() {
            "--" => end = true,
            "--cached" => cached = true,
            "--index" => indexed = true,
            "--check" => check = true,
            "-R" | "--reverse" => reverse = true,
            "--whitespace=nowarn" | "--whitespace=warn" | "--binary" => {}
            a if a.starts_with("-p") => strip = a[2..].parse()?,
            "-" => names.push("-"),
            a if a.starts_with('-') => bail!("unsupported apply option {a}"),
            a => names.push(a),
        }
    }
    if names.is_empty() {
        names.push("-");
    }
    let mut changes = Vec::new();
    let mut total = 0;
    for name in names {
        let bytes = read_input(cli, name)?;
        total += bytes.len();
        ensure!(total <= LIMIT, "patch inputs exceed limit");
        changes.extend(parse(&bytes, strip)?);
    }
    // Patches are relative to the repository root. Subdirectory invocations ignore other paths.
    let prefix = cli.prefix.as_os_str().as_encoded_bytes();
    if !prefix.is_empty() {
        changes.retain(|c| {
            c.old
                .iter()
                .chain(c.new.iter())
                .all(|p| p.starts_with(&[prefix, b"/"].concat()))
        });
    }
    apply_changes(repo, store, &changes, cached, indexed, check, reverse)
}

fn patch_between(store: &mut Store<'_>, before: &Entries, after: &Entries) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut budget = LIMIT;
    for path in before.keys().chain(after.keys()).collect::<BTreeSet<_>>() {
        let a = before.get(path).copied();
        let b = after.get(path).copied();
        if a == b {
            continue;
        }
        let ap = crate::output::quote(&[b"a/".as_slice(), path].concat());
        let bp = crate::output::quote(&[b"b/".as_slice(), path].concat());
        writeln!(out, "diff --git {ap} {bp}")?;
        match (a, b) {
            (None, Some(e)) => writeln!(out, "new file mode {:06o}", e.mode)?,
            (Some(e), None) => writeln!(out, "deleted file mode {:06o}", e.mode)?,
            (Some(a), Some(b)) if a.mode != b.mode => {
                writeln!(out, "old mode {:06o}\nnew mode {:06o}", a.mode, b.mode)?
            }
            _ => {}
        }
        if a.map(|e| e.oid) == b.map(|e| e.oid) {
            continue;
        }
        let old = crate::output::content(store, a, &mut budget)?;
        let new = crate::output::content(store, b, &mut budget)?;

        writeln!(
            out,
            "index {}..{}{}",
            a.map(|e| e.oid).unwrap_or(store.format().zero()),
            b.map(|e| e.oid).unwrap_or(store.format().zero()),
            a.zip(b)
                .filter(|(a, b)| a.mode == b.mode)
                .map(|(a, _)| format!(" {:06o}", a.mode))
                .unwrap_or_default()
        )?;
        if old.contains(&0) || new.contains(&0) {
            out.extend_from_slice(b"GIT binary patch\n");
            out.extend(crate::binary_patch::encode(&new)?);
            out.extend(crate::binary_patch::encode(&old)?);
            continue;
        }
        compat::diff_budget(&old, &new)?;
        let patch = diffy::create_patch_bytes(&old, &new).to_bytes();
        writeln!(
            out,
            "--- {}\n+++ {}",
            if a.is_some() {
                ap.as_str()
            } else {
                "/dev/null"
            },
            if b.is_some() {
                bp.as_str()
            } else {
                "/dev/null"
            }
        )?;
        for line in patch.split_inclusive(|b| *b == b'\n').skip(2) {
            out.extend_from_slice(line);
        }
        ensure!(out.len() <= LIMIT, "patch output exceeds limit");
    }
    Ok(out)
}
fn format_patch(cli: &Cli, repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    let (mut stdout, mut root) = (false, false);
    let mut max = None;
    let mut dir = ".";
    let mut rev = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--stdout" => stdout = true,
            "--root" => root = true,
            "-o" | "--output-directory" => dir = iter.next().context("missing output directory")?,
            "--no-signature" | "--full-index" | "--no-stat" | "--binary" => {}
            a if a.starts_with('-')
                && a.len() > 1
                && a[1..].bytes().all(|b| b.is_ascii_digit()) =>
            {
                max = Some(a[1..].parse::<usize>()?)
            }
            a if a.starts_with('-') => bail!("unsupported format-patch option {a}"),
            a => {
                ensure!(rev.is_none(), "format-patch accepts one revision range");
                rev = Some(a);
            }
        }
    }
    let value = rev.unwrap_or("HEAD");
    let range = if value.contains("..") || root || max.is_some() {
        value.to_owned()
    } else {
        format!("{value}..HEAD")
    };
    let mut ids = crate::inspect::history_ids(repo, store, &[&range], false, false)?;
    ids.retain(|id| store.parents(*id).is_ok_and(|p| p.len() <= 1));
    if let Some(max) = max {
        ids.truncate(max);
    }
    ids.reverse();
    let mut output = Vec::new();
    let mut total = 0;
    for (i, id) in ids.iter().enumerate() {
        let obj = store.get(*id)?;
        let author = porcelain::author(&obj.data)?;
        let message = porcelain::message(&obj.data)?;
        let mut fields = author.rsplitn(3, ' ');
        let zone = fields.next().context("invalid author")?;
        let stamp = fields.next().context("invalid author")?.parse::<i64>()?;
        let who = fields.next().context("invalid author")?;
        ensure!(
            zone.len() == 5 && matches!(zone.as_bytes()[0], b'+' | b'-'),
            "invalid author timezone"
        );
        let seconds = (zone[1..3].parse::<i32>()? * 60 + zone[3..].parse::<i32>()?)
            * 60
            * if zone.starts_with('-') { -1 } else { 1 };
        let date = time::OffsetDateTime::from_unix_timestamp(stamp)?
            .to_offset(time::UtcOffset::from_whole_seconds(seconds)?)
            .format(&time::format_description::well_known::Rfc2822)?;
        let msg = std::str::from_utf8(message)?;
        let (subject, body) = msg.split_once('\n').unwrap_or((msg, ""));
        ensure!(
            !who.chars().any(char::is_control) && !subject.chars().any(char::is_control),
            "invalid mail header"
        );
        let numbering = if ids.len() > 1 {
            format!(" {}/{}", i + 1, ids.len())
        } else {
            String::new()
        };
        let mut bytes=format!("From {id} Mon Sep 17 00:00:00 2001\nFrom: {who}\nDate: {date}\nSubject: [PATCH{numbering}] {subject}\nMIME-Version: 1.0\nContent-Type: text/plain; charset=UTF-8\nContent-Transfer-Encoding: 8bit\n\n{}\n---\n",body.trim_matches('\n')).into_bytes();
        let before = index::tree(store, obj.commit()?.1.first().copied())?;
        let after = porcelain::tree(store, *id)?;
        bytes.extend(patch_between(store, &before, &after)?);
        bytes.push(b'\n');
        total += bytes.len();
        ensure!(total <= LIMIT, "mailbox exceeds limit");
        let slug: String = subject
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .take(64)
            .collect();
        output.push((
            format!("{:04}-{}.patch", i + 1, slug.trim_matches('-')),
            bytes,
        ));
    }
    if stdout {
        let mut out = std::io::stdout().lock();
        for (_, bytes) in output {
            out.write_all(&bytes)?;
        }
        return Ok(());
    }
    // Relative output directories stay within the working tree; never follow repository symlinks.
    let specs = crate::workflow::paths(&[dir.to_owned()], &cli.prefix)?;
    let directory = &specs[0];
    let mut target = repo
        .worktree
        .as_ref()
        .context("format-patch output requires a worktree or --stdout")?
        .try_clone()?;
    for part in directory.split(|b| *b == b'/').filter(|v| !v.is_empty()) {
        match fs::mkdirat(&target, part, Mode::from_raw_mode(0o700)) {
            Ok(()) => {}
            Err(rustix::io::Errno::EXIST) => {}
            Err(e) => return Err(e.into()),
        }
        target = File::from(fs::openat(
            &target,
            part,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
    }
    for (name, bytes) in output {
        let mut file = File::from(fs::openat(
            &target,
            name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?);
        file.write_all(&bytes)?;
        file.sync_all()?;
        println!("{}", compat::text(&format!("{dir}/{name}")));
    }
    Ok(())
}
#[derive(serde::Serialize, serde::Deserialize)]
struct Letter {
    author: String,
    message: String,
    patch: Vec<u8>,
}
fn letters(bytes: &[u8]) -> Result<Vec<Letter>> {
    let normalized = std::str::from_utf8(bytes)
        .context("mailbox transport must be UTF-8 or MIME encoded")?
        .replace("\r\n", "\n");
    let text = normalized.as_str();
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.strip_prefix("From ").is_some_and(|v| {
            v.split_once(' ')
                .is_some_and(|(id, _)| Oid::parse(id.as_bytes()).is_ok())
        }) {
            starts.push(offset);
        }
        offset += line.len();
    }
    if starts.is_empty() {
        starts.push(0);
    }
    starts.push(text.len());
    let mut out = Vec::new();
    for pair in starts.windows(2) {
        let letter = &text[pair[0]..pair[1]];
        let letter = if letter.starts_with("From ") {
            letter.split_once('\n').context("empty email")?.1
        } else {
            letter
        };
        let (headers, body) = letter.split_once("\n\n").context("missing mail headers")?;
        let fields = crate::mime::headers(headers)?;
        let body = crate::mime::body(&fields, body)?;
        let who = crate::mime::header(fields.get("from").context("missing mail author")?)?;
        ensure!(
            !who.chars().any(char::is_control) && who.ends_with('>') && who.contains('<'),
            "invalid or encoded mail author"
        );
        let date = time::OffsetDateTime::parse(
            fields.get("date").context("missing mail date")?,
            &time::format_description::well_known::Rfc2822,
        )?;
        let seconds = date.offset().whole_seconds();
        let offset = format!(
            "{}{:02}{:02}",
            if seconds < 0 { '-' } else { '+' },
            seconds.abs() / 3600,
            seconds.abs() % 3600 / 60
        );
        let subject = crate::mime::header(fields.get("subject").context("missing mail subject")?)?;
        let subject = if subject.starts_with("[PATCH") {
            subject
                .split_once(']')
                .context("invalid patch subject")?
                .1
                .trim_start()
        } else {
            subject.as_str()
        };
        let mut msg = String::new();
        let mut patch_start = None;
        let mut pos = 0;
        for line in body.split_inclusive('\n') {
            if line.trim_end() == "---"
                || line.starts_with("diff --git ")
                || line.starts_with("--- ")
            {
                patch_start = Some(pos);
                break;
            }
            msg.push_str(line.trim_end());
            msg.push('\n');
            pos += line.len();
        }
        let patch = body.as_bytes()[patch_start.context("mail has no patch")?..].to_vec();
        parse(&patch, 1)?;
        let message = if msg.trim().is_empty() {
            format!("{subject}\n")
        } else {
            format!("{subject}\n\n{}\n", msg.trim_matches('\n'))
        };
        out.push(Letter {
            author: format!("{who} {} {offset}", date.unix_timestamp()),
            message,
            patch,
        });
        ensure!(out.len() <= 10_000, "mailbox has too many messages");
    }
    Ok(out)
}
const AM_STATE: &str = "safe-git-am";
#[derive(serde::Serialize, serde::Deserialize)]
struct MailState {
    branch: String,
    original: Oid,
    current: Oid,
    letters: Vec<Letter>,
    next: usize,
    applied: bool,
}
fn save_am(repo: &Repo, state: &MailState) -> Result<()> {
    let bytes = serde_json::to_vec(state)?;
    ensure!(bytes.len() <= 64 * 1024 * 1024, "am state exceeds limit");
    repo.atomic(AM_STATE, &bytes)
}
fn commit_mail(cli: &Cli, repo: &Repo, store: &mut Store<'_>, state: &mut MailState) -> Result<()> {
    let letter = state
        .letters
        .get(state.next)
        .context("invalid mail position")?;
    ensure!(
        repo.head()? == state.branch && repo.read_ref(&state.branch)? == Some(state.current),
        "HEAD changed during am"
    );
    let ref_lock = repo.prepare_ref(&state.branch, Some(state.current))?;
    let _head_lock = if state.branch != "HEAD" {
        Some(Pending::new(repo.root.try_clone()?, "HEAD.lock".into())?)
    } else {
        None
    };
    let _index_lock = Pending::new(repo.root.try_clone()?, "index.lock".into())?;
    let original_index = repo.read("index", checkout::INDEX_LIMIT)?;
    let staged = porcelain::index(repo)?;
    ensure!(
        checkout::snapshot(repo, &staged)? == staged,
        "unstaged changes remain"
    );
    let tree = porcelain::write_tree(repo, &staged)?;
    let id = porcelain::new_commit(
        repo,
        tree,
        &[state.current],
        &letter.author,
        &porcelain::identity(repo, &cli.config, "COMMITTER")?,
        letter.message.as_bytes(),
    )?;
    let logs = crate::history::logs(
        repo,
        &state.branch,
        Some(state.current),
        id,
        &format!("am: {}", porcelain::subject(&store.get(id)?.data)),
    )?;
    ensure!(
        repo.head()? == state.branch
            && repo.read("index", checkout::INDEX_LIMIT)? == original_index,
        "HEAD or index changed during am"
    );
    ref_lock.commit(id)?;
    crate::history::finish_logs(logs)?;
    state.current = id;
    state.next += 1;
    state.applied = false;
    save_am(repo, state)
}
fn am(cli: &Cli, repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    ensure!(
        !repo.exists("safe-git-sequence")?,
        "finish current history operation first"
    );
    let mut state = if let Some(bytes) = repo.read(AM_STATE, 64 * 1024 * 1024)? {
        let mut state: MailState = serde_json::from_slice(&bytes)?;
        crate::repo::reference(&state.branch)?;
        ensure!(
            state.next <= state.letters.len()
                && state.letters.len() <= 10_000
                && repo.head()? == state.branch,
            "invalid am state"
        );
        match args.first().map(String::as_str) {
            Some("--abort") if args.len() == 1 => {
                porcelain::reset(repo, store, state.original, cli.quiet)?;
                repo.remove(AM_STATE)?;
                return Ok(());
            }
            Some("--continue") if args.len() == 1 && state.next == state.letters.len() => {
                repo.remove(AM_STATE)?;
                return Ok(());
            }
            Some("--continue") if args.len() == 1 => {
                commit_mail(cli, repo, store, &mut state)?;
            }
            Some("--skip") if args.len() == 1 => {
                ensure!(
                    repo.read_ref(&state.branch)? == Some(state.current),
                    "HEAD changed during am"
                );
                porcelain::reset(repo, store, state.current, true)?;
                state.next += 1;
                state.applied = false;
                save_am(repo, &state)?;
            }
            _ => bail!("am in progress; use --continue, --skip, or --abort"),
        }
        state
    } else {
        let mut mail = Vec::new();
        let mut total = 0;
        let names = if args.is_empty() {
            vec!["-".to_owned()]
        } else {
            args.to_vec()
        };
        for name in names {
            ensure!(
                name == "-" || !name.starts_with('-'),
                "unsupported am option {name}"
            );
            let bytes = read_input(cli, &name)?;
            total += bytes.len();
            ensure!(total <= LIMIT, "mailbox exceeds limit");
            mail.extend(letters(&bytes)?);
        }
        ensure!(!mail.is_empty(), "empty mailbox");
        let current = repo
            .read_ref("HEAD")?
            .context("am requires an existing base commit")?;
        let staged = porcelain::index(repo)?;
        ensure!(
            staged == porcelain::tree(store, current)?
                && checkout::snapshot(repo, &staged)? == staged,
            "am requires a clean index and worktree"
        );
        let state = MailState {
            branch: repo.head()?,
            original: current,
            current,
            letters: mail,
            next: 0,
            applied: false,
        };
        save_am(repo, &state)?;
        state
    };
    while state.next < state.letters.len() {
        let changes = parse(&state.letters[state.next].patch, 1)?;
        if let Err(e) = apply_changes(repo, store, &changes, false, true, false, false) {
            bail!(
                "patch does not apply: {e:#}; resolve and add, then am --continue, or am --skip/--abort"
            );
        }
        state.applied = true;
        save_am(repo, &state)?;
        commit_mail(cli, repo, store, &mut state)?;
    }
    repo.remove(AM_STATE)?;
    Ok(())
}
