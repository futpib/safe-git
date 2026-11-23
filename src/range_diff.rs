use crate::{
    object::{Oid, ReadObjects},
    porcelain,
    repo::{Repo, Store},
};
use anyhow::{Result, ensure};
struct Patch {
    id: Oid,
    subject: String,
    patch_id: Oid,
    bytes: Vec<u8>,
}
fn series(repo: &Repo, store: &mut Store<'_>, range: &str) -> Result<Vec<Patch>> {
    ensure!(range.contains(".."), "range-diff expects commit ranges");
    let mut ids = crate::inspect::history_ids(repo, store, &[range], false, false)?;
    ensure!(ids.len() <= 1000, "range-diff commit budget exceeded");
    ids.reverse();
    let mut out = Vec::new();
    let mut total = 0;
    for id in ids {
        let obj = store.get(id)?;
        let parents = obj.commit()?.1;
        if parents.len() > 1 {
            continue;
        }
        let before = crate::index::tree(store, parents.first().copied())?;
        let after = porcelain::tree(store, id)?;
        let bytes = crate::mail::patch_between(store, &before, &after)?;
        total += bytes.len();
        ensure!(total <= 16 * 1024 * 1024, "range-diff byte budget exceeded");
        let normalized: Vec<u8> = bytes
            .split_inclusive(|b| *b == b'\n')
            .filter(|l| !l.starts_with(b"index ") && !l.starts_with(b"@@ "))
            .flatten()
            .copied()
            .collect();
        let patch_id = crate::object::hash_for(repo.format, &[&normalized])?;
        out.push(Patch {
            patch_id,
            id,
            subject: porcelain::subject(&obj.data),
            bytes,
        });
    }
    Ok(out)
}
pub fn run(repo: &Repo, store: &mut Store<'_>, args: &[String]) -> Result<()> {
    let mut values = Vec::new();
    let mut patches = true;
    for arg in args {
        match arg.as_str() {
            "--no-color" | "--color=never" => {}
            "--no-patch" | "-s" => patches = false,
            _ => {
                ensure!(!arg.starts_with('-'), "unsupported range-diff option {arg}");
                values.push(arg.clone());
            }
        }
    }
    if values.len() == 3 {
        values = vec![
            format!("{}..{}", values[0], values[1]),
            format!("{}..{}", values[0], values[2]),
        ];
    }
    ensure!(
        values.len() == 2,
        "range-diff requires OLD_RANGE NEW_RANGE or BASE OLD NEW"
    );
    let old = series(repo, store, &values[0])?;
    let new = series(repo, store, &values[1])?;
    let mut used = std::collections::BTreeSet::new();
    // Match patch identity before subjects, so reordered commits remain paired.
    let mut pairs = vec![None; old.len()];
    for exact in [true, false] {
        for (i, a) in old.iter().enumerate() {
            if pairs[i].is_some() {
                continue;
            }
            if let Some((j, _)) = new.iter().enumerate().find(|(j, b)| {
                !used.contains(j)
                    && if exact {
                        a.patch_id == b.patch_id
                    } else {
                        a.subject == b.subject
                    }
            }) {
                pairs[i] = Some(j);
                used.insert(j);
            }
        }
    }
    for (i, a) in old.iter().enumerate() {
        if let Some(j) = pairs[i] {
            let b = &new[j];
            let same = a.patch_id == b.patch_id && a.subject == b.subject;
            println!(
                "{}: {} {} {}: {} {}",
                i + 1,
                crate::output::short(a.id),
                if same { "=" } else { "!" },
                j + 1,
                crate::output::short(b.id),
                crate::compat::text(&b.subject)
            );
            if patches && !same {
                let left = [a.subject.as_bytes(), b"\n", &a.bytes].concat();
                let right = [b.subject.as_bytes(), b"\n", &b.bytes].concat();
                crate::compat::diff_budget(&left, &right)?;
                println!(
                    "{}",
                    crate::compat::text(&String::from_utf8_lossy(
                        &diffy::create_patch_bytes(&left, &right).to_bytes()
                    ))
                );
            }
        } else {
            println!(
                "{}: {} < -: ------- {}",
                i + 1,
                crate::output::short(a.id),
                crate::compat::text(&a.subject)
            );
        }
    }
    for (j, b) in new.iter().enumerate().filter(|(j, _)| !used.contains(j)) {
        println!(
            "-: ------- > {}: {} {}",
            j + 1,
            crate::output::short(b.id),
            crate::compat::text(&b.subject)
        );
    }
    Ok(())
}
