//! Built-in comparison options. Attributes and external diff commands are never consulted.
use crate::{
    compat,
    index::{Entries, Entry},
    object::ReadObjects,
    repo::Store,
};
use anyhow::{Context, Result, ensure};
use std::collections::BTreeSet;

pub fn whitespace(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .copied()
        .filter(|b| *b == b'\n' || !b.is_ascii_whitespace())
        .collect()
}
pub fn whitespace_patch(old: &[u8], new: &[u8]) -> Result<Vec<u8>> {
    let a = whitespace(old);
    let b = whitespace(new);
    compat::diff_budget(&a, &b)?;
    let patch = diffy::create_patch_bytes(&a, &b);
    let old_lines: Vec<_> = old.split_inclusive(|b| *b == b'\n').collect();
    let new_lines: Vec<_> = new.split_inclusive(|b| *b == b'\n').collect();
    let mut out = b"--- old\n+++ new\n".to_vec();
    for h in patch.hunks() {
        out.extend_from_slice(format!("@@ -{} +{} @@\n", h.old_range(), h.new_range()).as_bytes());
        let mut ai = if h.old_range().is_empty() {
            h.old_range().start()
        } else {
            h.old_range().start() - 1
        };
        let mut bi = if h.new_range().is_empty() {
            h.new_range().start()
        } else {
            h.new_range().start() - 1
        };
        for line in h.lines() {
            let (prefix, data) = match line {
                diffy::Line::Context(_) => {
                    let data = new_lines[bi];
                    ai += 1;
                    bi += 1;
                    (b' ', data)
                }
                diffy::Line::Delete(_) => {
                    let data = old_lines[ai];
                    ai += 1;
                    (b'-', data)
                }
                diffy::Line::Insert(_) => {
                    let data = new_lines[bi];
                    bi += 1;
                    (b'+', data)
                }
            };
            out.push(prefix);
            out.extend_from_slice(data);
            if !data.ends_with(b"\n") {
                out.extend_from_slice(b"\n\\ No newline at end of file\n");
            }
        }
    }
    Ok(out)
}

pub fn similarity(store: &mut Store<'_>, a: Entry, b: Entry) -> Result<usize> {
    if a.mode & 0o170000 != b.mode & 0o170000 {
        return Ok(0);
    }
    if a.oid == b.oid {
        return Ok(100);
    }
    if a.mode != 0o100644 && a.mode != 0o100755 {
        return Ok(0);
    }
    let a = store.get(a.oid)?;
    let b = store.get(b.oid)?;
    compat::diff_budget(&a.data, &b.data)?;
    let total = a.data.len().max(b.data.len());
    if total == 0 {
        return Ok(100);
    }
    let mut counts = std::collections::BTreeMap::new();
    for line in a.data.split_inclusive(|b| *b == b'\n') {
        *counts.entry(line).or_insert(0usize) += 1;
    }
    let mut common = 0;
    for line in b.data.split_inclusive(|b| *b == b'\n') {
        if let Some(n) = counts.get_mut(line).filter(|n| **n > 0) {
            *n -= 1;
            common += line.len();
        }
    }
    Ok(common * 100 / total)
}

pub type Rename = (Vec<u8>, Vec<u8>, usize);
pub fn renames(
    store: &mut Store<'_>,
    before: &Entries,
    after: &Entries,
    threshold: usize,
) -> Result<Vec<Rename>> {
    let old: Vec<_> = before
        .iter()
        .filter(|(p, _)| !after.contains_key(*p))
        .collect();
    let new: Vec<_> = after
        .iter()
        .filter(|(p, _)| !before.contains_key(*p))
        .collect();
    ensure!(
        old.len().saturating_mul(new.len()) <= 10000,
        "rename comparison budget exceeded"
    );
    let mut candidates = Vec::new();
    for (a, av) in &old {
        for (b, bv) in &new {
            let score = similarity(store, **av, **bv)?;
            if score >= threshold {
                candidates.push((std::cmp::Reverse(score), (*a).clone(), (*b).clone()));
            }
        }
    }
    candidates.sort();
    let (mut used_old, mut used_new) = (BTreeSet::new(), BTreeSet::new());
    let mut result = Vec::new();
    for (std::cmp::Reverse(score), a, b) in candidates {
        if !used_old.contains(&a) && !used_new.contains(&b) {
            used_old.insert(a.clone());
            used_new.insert(b.clone());
            result.push((a, b, score));
        }
    }
    Ok(result)
}

pub fn rename_threshold(items: &[String]) -> Result<Option<usize>> {
    let Some(arg) = items
        .iter()
        .find(|s| s.starts_with("-M") || s.starts_with("--find-renames"))
    else {
        return Ok(None);
    };
    let value = arg
        .strip_prefix("--find-renames=")
        .or_else(|| arg.strip_prefix("-M"))
        .unwrap_or("");
    if value.is_empty() {
        return Ok(Some(50));
    }
    let n = if let Some(percent) = value.strip_suffix('%') {
        percent.parse::<usize>()?
    } else {
        ensure!(
            value.bytes().all(|b| b.is_ascii_digit()) && value.len() <= 3,
            "invalid rename threshold"
        );
        value.parse::<usize>()? * 100 / 10usize.pow(value.len() as u32)
    };
    ensure!(
        (1..=100).contains(&n),
        "rename threshold must be between 1 and 100 percent"
    );
    Ok(Some(n))
}

fn words(bytes: &[u8]) -> Vec<&[u8]> {
    let mut spans = Vec::new();
    let mut start = 0;
    for i in 1..bytes.len() {
        if bytes[i - 1].is_ascii_whitespace() && !bytes[i].is_ascii_whitespace() {
            spans.push(&bytes[start..i]);
            start = i;
        }
    }
    if start < bytes.len() {
        spans.push(&bytes[start..]);
    }
    spans
}
pub fn word_diff(old: &[u8], new: &[u8]) -> Result<String> {
    let a = words(old);
    let b = words(new);
    let tokens = |v: &[&[u8]]| {
        v.iter()
            .flat_map(|s| {
                s.iter()
                    .copied()
                    .filter(|c| !c.is_ascii_whitespace())
                    .chain(*b"\n")
            })
            .collect::<Vec<_>>()
    };
    let aa = tokens(&a);
    let bb = tokens(&b);
    compat::diff_budget(&aa, &bb)?;
    let mut options = diffy::DiffOptions::new();
    options.set_context_len(0);
    let patch = options.create_patch_bytes(&aa, &bb);
    let mut out = String::new();
    let mut cursor = 0;
    for h in patch.hunks() {
        let from = if h.new_range().is_empty() {
            h.new_range().start()
        } else {
            h.new_range().start() - 1
        };
        for span in &b[cursor..from] {
            out.push_str(&compat::text(&String::from_utf8_lossy(span)));
        }
        let ai = if h.old_range().is_empty() {
            h.old_range().start()
        } else {
            h.old_range().start() - 1
        };
        let deleted = a[ai..ai + h.old_range().len()].concat();
        let added = b[from..from + h.new_range().len()].concat();
        if !deleted.is_empty() {
            out.push_str(&format!(
                "[-{}-]",
                compat::text(String::from_utf8_lossy(&deleted).trim_end())
            ));
        }
        if !added.is_empty() {
            out.push_str(&format!(
                "{{+{}+}}",
                compat::text(String::from_utf8_lossy(&added).trim_end())
            ));
        }
        let trailing = if added.is_empty() { &deleted } else { &added };
        let end = trailing
            .iter()
            .rposition(|c| !c.is_ascii_whitespace())
            .map_or(0, |n| n + 1);
        out.push_str(&String::from_utf8_lossy(&trailing[end..]));
        cursor = from + h.new_range().len();
    }
    for span in &b[cursor..] {
        out.push_str(&compat::text(&String::from_utf8_lossy(span)));
    }
    Ok(out)
}

pub fn since(value: &str) -> Result<i64> {
    if let Some(n) = value.strip_prefix('@') {
        return Ok(n.parse()?);
    }
    if let Ok(date) =
        time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
    {
        return Ok(date.unix_timestamp());
    }
    let parts: Vec<_> = value.split('-').collect();
    if parts.len() == 3 {
        let date = time::Date::from_calendar_date(
            parts[0].parse()?,
            time::Month::try_from(parts[1].parse::<u8>()?)?,
            parts[2].parse()?,
        )?;
        return Ok(date.midnight().assume_utc().unix_timestamp());
    }
    let parts: Vec<_> = value.split_whitespace().collect();
    if parts.len() == 3 && parts[2] == "ago" {
        let count: i64 = parts[0].parse()?;
        let unit = match parts[1].trim_end_matches('s') {
            "second" => 1,
            "minute" => 60,
            "hour" => 3600,
            "day" => 86400,
            "week" => 604800,
            _ => anyhow::bail!("unsupported relative date unit"),
        };
        let delta = count.checked_mul(unit).context("date overflow")?;
        return time::OffsetDateTime::now_utc()
            .unix_timestamp()
            .checked_sub(delta)
            .context("date overflow");
    }
    anyhow::bail!("date must be YYYY-MM-DD, RFC3339, @SECONDS, or 'N days ago'")
}
