//! Git bundle v2/v3 data with verified packs and bounded prerequisite traversal.
use crate::{
    Cli,
    object::{self, Oid, ReadObjects},
    repo::{Repo, Store},
    revision,
};
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, HashSet};
pub fn run(cli: &Cli, repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    ensure!(
        args.len() >= 2,
        "bundle requires create/verify/list-heads/unbundle FILE"
    );
    let action = args[0].as_str();
    if action == "create" {
        ensure!(repo.shallow.is_empty(), "cannot bundle shallow history");
        let mut refs = BTreeMap::new();
        let mut prerequisites = Vec::new();
        for value in &args[2..] {
            if value == "--all" {
                refs.extend(repo.refs()?);
                if let Some(head) = repo.read_ref("HEAD")? {
                    refs.insert("HEAD".into(), head);
                }
                continue;
            }
            let mut value = value.as_str();
            if let Some((base, tip)) = value.split_once("..") {
                ensure!(!tip.starts_with('.'), "bundle requires two-dot range");
                prerequisites.push(revision::commit(repo, store, base)?);
                value = tip;
            } else if let Some(base) = value.strip_prefix('^') {
                prerequisites.push(revision::commit(repo, store, base)?);
                continue;
            }
            ensure!(!value.starts_with('-'), "unsupported bundle create option");
            let id = revision::resolve(repo, store, value)?;
            let name = if value == "HEAD" || value.starts_with("refs/") {
                value.to_owned()
            } else if repo.read_ref(&format!("refs/heads/{value}"))? == Some(id) {
                format!("refs/heads/{value}")
            } else {
                format!("refs/tags/{value}")
            };
            crate::repo::reference(&name)?;
            refs.insert(name, id);
        }
        ensure!(!refs.is_empty(), "bundle needs named revisions or --all");
        let boundary = object::commits(store, &prerequisites)?;
        let ids = object::reachable_many_since(
            store,
            &refs.values().copied().collect::<Vec<_>>(),
            &boundary,
        )?;
        let objects = ids
            .into_iter()
            .map(|id| store.get(id))
            .collect::<Result<Vec<_>>>()?;
        let mut bytes = if repo.format == object::Format::Sha1 {
            b"# v2 git bundle\n".to_vec()
        } else {
            b"# v3 git bundle\n@object-format=sha256\n".to_vec()
        };
        for id in prerequisites {
            bytes.extend_from_slice(format!("-{id} prerequisite\n").as_bytes());
        }
        for (name, id) in refs {
            bytes.extend_from_slice(format!("{id} {name}\n").as_bytes());
        }
        bytes.push(b'\n');
        bytes.extend(crate::pack::encode_for(&objects, repo.format)?);
        return crate::extras::write_file(cli, &args[1], &bytes);
    }
    ensure!(
        matches!(action, "verify" | "list-heads" | "unbundle"),
        "unsupported bundle command"
    );
    let bytes = crate::mail::read_input_limit(cli, &args[1], object::MAX_PACK + 8 * 1024 * 1024)?;
    let end = bytes
        .windows(2)
        .position(|w| w == b"\n\n")
        .context("missing bundle header terminator")?;
    ensure!(end <= 8 * 1024 * 1024, "bundle header exceeds limit");
    let mut lines = std::str::from_utf8(&bytes[..end])?.lines();
    let signature = lines.next().context("missing bundle signature")?;
    ensure!(
        matches!(signature, "# v2 git bundle" | "# v3 git bundle"),
        "invalid bundle signature"
    );
    let mut format = object::Format::Sha1;
    let mut refs = BTreeMap::new();
    let mut prerequisites = Vec::new();
    for line in lines {
        if let Some(value) = line.strip_prefix("@object-format=") {
            ensure!(
                signature == "# v3 git bundle" && refs.is_empty() && prerequisites.is_empty(),
                "invalid bundle capability position"
            );
            format = object::Format::parse(value)?;
            continue;
        }
        ensure!(!line.starts_with('@'), "unsupported bundle capability");
        let (id, name) = line.split_once(' ').context("invalid bundle ref")?;
        if let Some(id) = id.strip_prefix('-') {
            prerequisites.push(Oid::parse(id.as_bytes())?);
        } else {
            let id = Oid::parse(id.as_bytes())?;
            crate::repo::reference(name)?;
            ensure!(
                refs.insert(name.to_owned(), id).is_none(),
                "duplicate bundle ref"
            );
        }
        ensure!(
            refs.len() + prerequisites.len() <= object::MAX_OBJECTS,
            "bundle header exceeds limit"
        );
    }
    ensure!(
        format == repo.format
            && refs
                .values()
                .chain(&prerequisites)
                .all(|id| id.format() == format),
        "bundle object format differs"
    );
    if action != "list-heads" {
        ensure!(args.len() == 2, "unexpected bundle arguments");
        store.allow_promisor = false;
        for id in &prerequisites {
            store
                .get(*id)?
                .commit()
                .context("missing prerequisite commit")?;
        }
        let mut bases = object::Objects::new();
        for id in object::reachable_many_since(store, &prerequisites, &HashSet::new())? {
            bases.insert(id, store.get(id)?);
        }
        let objects = crate::pack::decode_with(&bytes[end + 2..], format, &bases)?;
        // Validate connectivity before writing any object or reference.
        struct Overlay<'a, 'b> {
            incoming: &'a object::Objects,
            store: &'a mut Store<'b>,
        }
        impl ReadObjects for Overlay<'_, '_> {
            fn format(&self) -> object::Format {
                self.store.format()
            }
            fn get(&mut self, id: Oid) -> Result<std::sync::Arc<object::Object>> {
                self.incoming
                    .get(&id)
                    .cloned()
                    .map(Ok)
                    .unwrap_or_else(|| self.store.get(id))
            }
        }
        let mut overlay = Overlay {
            incoming: &objects,
            store,
        };
        object::reachable_many_since(
            &mut overlay,
            &refs.values().copied().collect::<Vec<_>>(),
            &HashSet::new(),
        )?;
        if action == "unbundle" {
            for object in objects.values() {
                repo.write_object(object)?;
            }
            store.add(objects)?;
        }
    }
    if action == "verify" {
        println!("Bundle verified ({} refs)", refs.len());
    } else {
        for (name, id) in refs {
            if args.len() == 2 || args[2..].contains(&name) {
                println!("{id} {name}");
            }
        }
    }
    Ok(())
}
