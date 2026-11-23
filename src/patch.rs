//! Built-in hunk selection. No pager, editor, filter, or command can be selected by data.
use crate::{
    compat,
    index::{Entries, Entry},
    object::{Kind, Object, ReadObjects},
    repo::{Repo, Store},
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeSet,
    io::{BufRead, Read, Write},
};
fn answer(prompt: &str, rest: &mut Option<bool>) -> Result<bool> {
    if let Some(value) = rest {
        return Ok(*value);
    }
    loop {
        print!("{prompt} [y,n,a,d,q] ");
        std::io::stdout().flush()?;
        let mut bytes = Vec::new();
        let n = std::io::stdin()
            .lock()
            .take(4097)
            .read_until(b'\n', &mut bytes)?;
        ensure!(n > 0 && n <= 4096, "missing or oversized patch answer");
        match bytes.first() {
            Some(b'y') => return Ok(true),
            Some(b'n') => return Ok(false),
            Some(b'a') => {
                *rest = Some(true);
                return Ok(true);
            }
            Some(b'd' | b'q') => {
                *rest = Some(false);
                return Ok(false);
            }
            _ => println!("Choose y, n, a, d, or q."),
        }
    }
}
fn bytes(store: &mut Store<'_>, entry: Option<Entry>) -> Result<Vec<u8>> {
    match entry {
        None => Ok(Vec::new()),
        Some(e) => {
            let o = store.get(e.oid)?;
            ensure!(o.kind == Kind::Blob, "patch requires blobs");
            Ok(o.data.clone())
        }
    }
}
pub fn select(
    repo: &Repo,
    store: &mut Store<'_>,
    before: &Entries,
    after: &Entries,
) -> Result<Entries> {
    let mut result = before.clone();
    let mut rest = None;
    let paths: BTreeSet<_> = before.keys().chain(after.keys()).collect();
    for path in paths {
        let a = before.get(path).copied();
        let b = after.get(path).copied();
        if a == b {
            continue;
        }
        println!(
            "diff --git a/{} b/{}",
            compat::text(&String::from_utf8_lossy(path)),
            compat::text(&String::from_utf8_lossy(path))
        );
        if a.is_some_and(|e| !matches!(e.mode, 0o100644 | 0o100755))
            || b.is_some_and(|e| !matches!(e.mode, 0o100644 | 0o100755))
            || a.map(|e| e.mode) != b.map(|e| e.mode)
        {
            if answer("Apply whole file change?", &mut rest)? {
                if let Some(b) = b {
                    result.insert(path.clone(), b);
                } else {
                    result.remove(path);
                }
            }
            continue;
        }
        let old = bytes(store, a)?;
        let new = bytes(store, b)?;
        compat::diff_budget(&old, &new)?;
        if old.contains(&0) || new.contains(&0) {
            if answer("Apply binary change?", &mut rest)? {
                if let Some(b) = b {
                    result.insert(path.clone(), b);
                } else {
                    result.remove(path);
                }
            }
            continue;
        }
        let patch = diffy::create_patch_bytes(&old, &new);
        let lines: Vec<_> = old.split_inclusive(|b| *b == b'\n').collect();
        let mut data = Vec::new();
        let mut cursor = 0;
        for hunk in patch.hunks() {
            let start = if hunk.old_range().is_empty() {
                hunk.old_range().start()
            } else {
                hunk.old_range().start() - 1
            };
            for line in &lines[cursor..start] {
                data.extend_from_slice(line);
            }
            println!("@@ -{} +{} @@", hunk.old_range(), hunk.new_range());
            for line in hunk.lines() {
                let (prefix, bytes) = match line {
                    diffy::Line::Context(v) => (' ', *v),
                    diffy::Line::Delete(v) => ('-', *v),
                    diffy::Line::Insert(v) => ('+', *v),
                };
                print!("{prefix}{}", compat::text(&String::from_utf8_lossy(bytes)));
            }
            let yes = answer("Apply this hunk?", &mut rest)?;
            for line in hunk.lines() {
                match line {
                    diffy::Line::Context(v) => data.extend_from_slice(v),
                    diffy::Line::Insert(v) if yes => data.extend_from_slice(v),
                    diffy::Line::Delete(v) if !yes => data.extend_from_slice(v),
                    _ => {}
                }
            }
            cursor = start + hunk.old_range().len();
        }
        for line in &lines[cursor..] {
            data.extend_from_slice(line);
        }
        let object = Object {
            kind: Kind::Blob,
            data,
        };
        let oid = object.oid()?;
        repo.write_object(&object)?;
        result.insert(
            path.clone(),
            Entry {
                flags: 0,
                mode: a.or(b).context("missing patch entry")?.mode,
                oid,
            },
        );
    }
    Ok(result)
}
