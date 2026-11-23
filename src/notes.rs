use crate::{
    Cli,
    index::Entry,
    object::{Kind, Object, ReadObjects},
    porcelain,
    repo::{Repo, Store},
    revision,
};
use anyhow::{Context, Result, bail, ensure};
pub fn run(cli: &Cli, repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    let mut reference = "refs/notes/commits".to_owned();
    let mut message: Option<Vec<u8>> = None;
    let mut force = false;
    let mut values = Vec::new();
    let mut items = args.iter();
    while let Some(a) = items.next() {
        match a.as_str() {
            "--ref" => reference = items.next().context("missing notes ref")?.clone(),
            a if a.starts_with("--ref=") => reference = a[6..].into(),
            "-f" | "--force" => force = true,
            "-m" | "--message" => {
                let text = items.next().context("missing note message")?;
                let bytes = message.get_or_insert_with(Vec::new);
                if !bytes.is_empty() {
                    bytes.push(b'\n');
                }
                bytes.extend_from_slice(text.as_bytes());
                bytes.push(b'\n');
            }
            "-F" | "--file" => {
                message = Some(crate::mail::read_input(
                    cli,
                    items.next().context("missing message file")?,
                )?)
            }
            a if a.starts_with('-') => bail!("unsupported notes option {a}"),
            _ => values.push(a.as_str()),
        }
    }
    if !reference.starts_with("refs/notes/") {
        reference = format!("refs/notes/{reference}");
    }
    crate::repo::refname(&reference)?;
    let action = values.first().copied().unwrap_or("list");
    let args = values.get(1..).unwrap_or_default();
    ensure!(args.len() <= 2, "too many notes arguments");
    let old = repo.read_ref(&reference)?;
    let mut entries = crate::index::tree(store, old)?;
    let find = |entries: &crate::index::Entries, id: crate::object::Oid| {
        entries
            .keys()
            .find(|p| {
                p.iter().copied().filter(|b| *b != b'/').collect::<Vec<_>>()
                    == id.to_string().as_bytes()
            })
            .cloned()
    };
    if action == "list" && args.is_empty() {
        for (path, e) in &entries {
            let id: Vec<_> = path.iter().copied().filter(|b| *b != b'/').collect();
            if let Ok(id) = crate::object::Oid::parse(&id) {
                println!("{} {id}", e.oid);
            }
        }
        return Ok(());
    }
    let id = revision::resolve(repo, store, args.first().copied().unwrap_or("HEAD"))?;
    let path = find(&entries, id).unwrap_or_else(|| id.to_string().into_bytes());
    match action {
        "show" | "list" => {
            let e = entries.get(&path).context("no note for this object")?;
            if action == "list" {
                println!("{}", e.oid);
            } else {
                let obj = store.get(e.oid)?;
                ensure!(obj.kind == Kind::Blob, "note is not a blob");
                print!(
                    "{}",
                    crate::compat::text(&String::from_utf8_lossy(&obj.data))
                );
            }
            return Ok(());
        }
        "remove" => {
            ensure!(entries.remove(&path).is_some(), "no note for this object");
        }
        "copy" => {
            ensure!(args.len() == 2, "notes copy requires FROM TO");
            let e = *entries.get(&path).context("source has no note")?;
            let dest = revision::resolve(repo, store, args[1])?;
            let dest = find(&entries, dest).unwrap_or_else(|| dest.to_string().into_bytes());
            ensure!(
                force || !entries.contains_key(&dest),
                "destination already has a note; use -f"
            );
            entries.insert(dest, e);
        }
        "add" | "append" | "edit" => {
            ensure!(args.len() <= 1, "notes takes one object");
            ensure!(
                action != "add" || force || !entries.contains_key(&path),
                "note exists; use -f"
            );
            let mut data = message.context("provide -m or -F; editors are never run")?;
            if action == "append"
                && let Some(e) = entries.get(&path)
            {
                let obj = store.get(e.oid)?;
                ensure!(obj.kind == Kind::Blob, "note is not a blob");
                let mut prior = obj.data.clone();
                prior.push(b'\n');
                prior.append(&mut data);
                data = prior;
            }
            ensure!(
                data.len() <= crate::object::MAX_OBJECT,
                "note exceeds limit"
            );
            let obj = Object {
                kind: Kind::Blob,
                data,
            };
            repo.write_object(&obj)?;
            entries.insert(
                path,
                Entry {
                    mode: 0o100644,
                    flags: 0,
                    oid: obj.oid_for(repo.format)?,
                },
            );
        }
        _ => bail!("unsupported notes command {action}"),
    }
    let lock = repo.prepare_ref(&reference, old)?;
    let tree = porcelain::write_tree(repo, &entries)?;
    let who = porcelain::identity(repo, &cli.config, "COMMITTER")?;
    let id = porcelain::new_commit(
        repo,
        tree,
        &old.into_iter().collect::<Vec<_>>(),
        &who,
        &who,
        b"Update notes",
    )?;
    lock.commit(id)
}
