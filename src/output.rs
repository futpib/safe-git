//! Git's plain, English output for the supported subset. No external diff,
//! attributes, textconv, pager, terminal controls, or configuration evaluation.
use crate::{
    index::{self, Entry},
    object::{Kind, Oid, ReadObjects},
};
use anyhow::{Result, ensure};
use std::{
    collections::{BTreeSet, HashMap},
    fmt::Write,
};

pub fn short(oid: Oid) -> String {
    oid.to_string()[..7].to_owned()
}

fn display_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("ssh://") {
        format!(
            "ssh://{}",
            rest.split_once('@').map_or(rest, |(_, host)| host)
        )
    } else {
        url.split_once('@').map_or(url, |(_, host)| host).to_owned()
    }
}

pub fn fetch(
    url: &str,
    branch: &str,
    remote: &str,
    old: Option<Oid>,
    new: Oid,
    ff: bool,
) -> String {
    if old == Some(new) {
        return String::new();
    }
    let (flag, summary, suffix) = match old {
        None => ('*', "[new branch]".to_owned(), ""),
        Some(old) if ff => (' ', format!("{}..{}", short(old), short(new)), ""),
        Some(old) => (
            '+',
            format!("{}...{}", short(old), short(new)),
            "  (forced update)",
        ),
    };
    let url = url.trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let url = display_url(url);
    format!("From {url}\n {flag} {summary:17} {branch:10} -> {remote}/{branch}{suffix}\n")
}

pub fn push(url: &str, local: &str, remote: &str, old: Option<Oid>, new: Oid) -> String {
    let url = display_url(url);
    let (flag, summary) = match old {
        None => ('*', "[new branch]".to_owned()),
        Some(old) => (' ', format!("{}..{}", short(old), short(new))),
    };
    format!("To {url}\n {flag} {summary:17} {local} -> {remote}\n")
}

pub fn pull_fetch(url: &str, branch: &str) -> String {
    let url = url.trim_end_matches('/');
    let url = display_url(url.strip_suffix(".git").unwrap_or(url));
    format!("From {url}\n * branch            {branch:10} -> FETCH_HEAD\n")
}

pub(crate) fn quote(path: &[u8]) -> String {
    if path
        .iter()
        .all(|b| (32..127).contains(b) && !b"\"\\".contains(b))
    {
        return String::from_utf8(path.to_vec()).expect("ASCII path");
    }
    let mut out = String::from("\"");
    for &b in path {
        match b {
            b'"' | b'\\' => {
                out.push('\\');
                out.push(b as char);
            }
            32..=126 => out.push(b as char),
            _ => {
                write!(out, "\\{b:03o}").unwrap();
            }
        }
    }
    out.push('"');
    out
}

// A bounded exact line edit count. Hash-map interning avoids repeatedly comparing
// long lines inside the LCS loop. Exhausting the budget omits the optional stat,
// never invents approximate counts or prevents an otherwise valid checkout.
fn counts(old: &[u8], new: &[u8], budget: &mut usize) -> Result<(usize, usize)> {
    let a: Vec<_> = old.split_inclusive(|b| *b == b'\n').collect();
    let b: Vec<_> = new.split_inclusive(|b| *b == b'\n').collect();
    ensure!(a.len() + b.len() <= 200_000, "diffstat line limit exceeded");
    let prefix = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
    let a = &a[prefix..];
    let b = &b[prefix..];
    let suffix = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let a = &a[..a.len() - suffix];
    let b = &b[..b.len() - suffix];
    let cost = a.len().saturating_mul(b.len());
    ensure!(cost <= *budget, "diffstat comparison limit exceeded");
    *budget -= cost;
    if a.is_empty() || b.is_empty() {
        return Ok((b.len(), a.len()));
    }
    let mut lines = HashMap::new();
    let mut intern = |s| {
        let next = lines.len();
        *lines.entry(s).or_insert(next)
    };
    let a: Vec<usize> = a.iter().map(|s| intern(*s)).collect();
    let b: Vec<usize> = b.iter().map(|s| intern(*s)).collect();
    let mut row = vec![0usize; b.len() + 1];
    for old in &a {
        let mut diagonal = 0;
        for (j, new) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = if old == new {
                diagonal + 1
            } else {
                above.max(row[j])
            };
            diagonal = above;
        }
    }
    let common = row[b.len()];
    Ok((b.len() - common, a.len() - common))
}

struct Stat {
    name: String,
    old: Option<Entry>,
    new: Option<Entry>,
    add: usize,
    del: usize,
    binary: bool,
}

pub(crate) fn content(
    store: &mut impl ReadObjects,
    entry: Option<Entry>,
    budget: &mut usize,
) -> Result<Vec<u8>> {
    let Some(entry) = entry else {
        return Ok(Vec::new());
    };
    if entry.mode == 0o160000 {
        return Ok(format!("Subproject commit {}\n", entry.oid).into_bytes());
    }
    let object = store.get(entry.oid)?;
    ensure!(object.kind == Kind::Blob, "expected blob for diffstat");
    ensure!(object.data.len() <= *budget, "diffstat byte limit exceeded");
    *budget -= object.data.len();
    Ok(object.data.clone())
}

pub fn diffstat(store: &mut impl ReadObjects, old: Option<Oid>, new: Oid) -> Result<String> {
    let old_tree = index::tree(store, old)?;
    let new_tree = index::tree(store, Some(new))?;
    diffstat_entries(store, &old_tree, &new_tree)
}
pub(crate) fn diffstat_entries(
    store: &mut impl ReadObjects,
    old_tree: &index::Entries,
    new_tree: &index::Entries,
) -> Result<String> {
    let paths: BTreeSet<_> = old_tree.keys().chain(new_tree.keys()).collect();
    let mut bytes = 16 * 1024 * 1024;
    let mut comparisons = 8_000_000;
    let mut stats = Vec::new();
    for path in paths {
        let old = old_tree.get(path).copied();
        let new = new_tree.get(path).copied();
        if old == new {
            continue;
        }
        ensure!(stats.len() < 10_000, "diffstat file limit exceeded");
        let (add, del, binary) = if old.map(|e| e.oid) == new.map(|e| e.oid) {
            (0, 0, false)
        } else {
            let a = content(store, old, &mut bytes)?;
            let b = content(store, new, &mut bytes)?;
            let binary = a[..a.len().min(8000)].contains(&0) || b[..b.len().min(8000)].contains(&0);
            if binary {
                (b.len(), a.len(), true)
            } else {
                let (add, del) = counts(&a, &b, &mut comparisons)?;
                (add, del, false)
            }
        };
        stats.push(Stat {
            name: quote(path),
            old,
            new,
            add,
            del,
            binary,
        });
    }
    Ok(render(&stats))
}

fn render(stats: &[Stat]) -> String {
    if stats.is_empty() {
        return String::new();
    }
    let max_change = stats
        .iter()
        .filter(|s| !s.binary)
        .map(|s| s.add + s.del)
        .max()
        .unwrap_or(0);
    let number_width = max_change
        .to_string()
        .len()
        .max(if stats.iter().any(|s| s.binary) { 3 } else { 1 });
    let binary_width = stats
        .iter()
        .filter(|s| s.binary)
        .map(|s| 14 + s.add.to_string().len() + s.del.to_string().len())
        .max()
        .unwrap_or(0);
    let mut name_width = stats.iter().map(|s| s.name.len()).max().unwrap_or(0);
    let mut graph_width = max_change.max(binary_width.saturating_sub(4));
    if name_width + number_width + 6 + graph_width > 80 {
        graph_width = graph_width.min((30usize - number_width - 6).max(6));
        if name_width > 80 - number_width - 6 - graph_width {
            name_width = 80 - number_width - 6 - graph_width;
        } else {
            graph_width = 80 - number_width - 6 - name_width;
        }
    }
    let mut out = String::new();
    let (mut additions, mut deletions) = (0, 0);
    for stat in stats {
        let mut name = stat.name.clone();
        if name.len() > name_width {
            let tail = &name[name.len() - name_width + 3..];
            name = format!("...{}", tail.find('/').map_or(tail, |i| &tail[i..]));
        }
        write!(out, " {name:name_width$} | ").unwrap();
        if stat.binary {
            writeln!(
                out,
                "{:number_width$} {} -> {} bytes",
                "Bin", stat.del, stat.add
            )
            .unwrap();
        } else {
            let (mut plus, mut minus) = (stat.add, stat.del);
            if max_change >= graph_width && max_change != 0 {
                let scale = |v: usize| {
                    if v == 0 {
                        0
                    } else {
                        1 + v * (graph_width - 1) / max_change
                    }
                };
                let total = scale(plus + minus).max(if plus != 0 && minus != 0 { 2 } else { 0 });
                if plus < minus {
                    plus = scale(plus);
                    minus = total - plus;
                } else {
                    minus = scale(minus);
                    plus = total - minus;
                }
            }
            write!(out, "{:number_width$}", stat.add + stat.del).unwrap();
            if plus + minus != 0 {
                write!(out, " {}{}", "+".repeat(plus), "-".repeat(minus)).unwrap();
            }
            out.push('\n');
            additions += stat.add;
            deletions += stat.del;
        }
    }
    write!(
        out,
        " {} file{} changed",
        stats.len(),
        if stats.len() == 1 { "" } else { "s" }
    )
    .unwrap();
    if additions != 0 || deletions == 0 {
        write!(
            out,
            ", {additions} insertion{}(+)",
            if additions == 1 { "" } else { "s" }
        )
        .unwrap();
    }
    if deletions != 0 || additions == 0 {
        write!(
            out,
            ", {deletions} deletion{}(-)",
            if deletions == 1 { "" } else { "s" }
        )
        .unwrap();
    }
    out.push('\n');
    for s in stats {
        match (s.old, s.new) {
            (None, Some(e)) => writeln!(out, " create mode {:06o} {}", e.mode, s.name).unwrap(),
            (Some(e), None) => writeln!(out, " delete mode {:06o} {}", e.mode, s.name).unwrap(),
            (Some(a), Some(b)) if a.mode != b.mode => writeln!(
                out,
                " mode change {:06o} => {:06o} {}",
                a.mode, b.mode, s.name
            )
            .unwrap(),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn line_counts_and_budget() {
        assert_eq!(
            counts(b"one\ntwo\n", b"three\ntwo\nfour\n", &mut 100).unwrap(),
            (2, 1)
        );
        assert_eq!(counts(b"", b"line", &mut 0).unwrap(), (1, 0));
        assert_eq!(counts(b"line\n", b"line", &mut 1).unwrap(), (1, 1));
        assert!(counts(b"a\nb\n", b"c\nd\n", &mut 3).is_err());
        assert_eq!(quote(b"a\xc3\xa9"), "\"a\\303\\251\"");
    }
}
