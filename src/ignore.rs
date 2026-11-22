//! Bounded gitignore matching. Patterns are data; no config includes or helpers.
use anyhow::{Result, ensure};

#[derive(Clone)]
pub struct Rule {
    base: Vec<u8>,
    pattern: Vec<u8>,
    negative: bool,
    directory: bool,
    anchored: bool,
}
pub fn extend(rules: &mut Vec<Rule>, base: &[u8], bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() <= 1024 * 1024, "ignore file exceeds limit");
    for line in bytes.split(|b| *b == b'\n') {
        let mut line = line.strip_suffix(b"\r").unwrap_or(line);
        while line.ends_with(b" ") {
            let escapes = line[..line.len() - 1]
                .iter()
                .rev()
                .take_while(|b| **b == b'\\')
                .count();
            if escapes % 2 == 1 {
                break;
            }
            line = &line[..line.len() - 1];
        }
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        let negative = line.starts_with(b"!");
        if negative {
            line = &line[1..];
        }
        let directory = line.ends_with(b"/");
        if directory {
            line = &line[..line.len() - 1];
        }
        let anchored = line.contains(&b'/');
        line = line.strip_prefix(b"/").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        ensure!(!line.contains(&0), "NUL in ignore pattern");
        ensure!(
            !line.windows(2).any(|w| w == b"[:"),
            "POSIX character classes in ignore patterns are unsupported"
        );
        ensure!(
            line.len() <= 4096 && rules.len() < 8192,
            "ignore pattern limit exceeded"
        );
        rules.push(Rule {
            base: base.to_vec(),
            pattern: line.to_vec(),
            negative,
            directory,
            anchored,
        });
    }
    Ok(())
}
pub fn ignored(rules: &[Rule], path: &[u8], directory: bool, budget: &mut usize) -> Result<bool> {
    let mut ignored = false;
    for rule in rules {
        if rule.directory && !directory {
            continue;
        }
        let Some(relative) = path.strip_prefix(rule.base.as_slice()) else {
            continue;
        };
        let relative = if rule.base.is_empty() {
            relative
        } else {
            let Some(relative) = relative.strip_prefix(b"/") else {
                continue;
            };
            relative
        };
        let value = if rule.anchored {
            relative
        } else {
            relative.rsplit(|b| *b == b'/').next().unwrap()
        };
        if matches(&rule.pattern, value, budget)? {
            ignored = !rule.negative;
        }
    }
    Ok(ignored)
}
fn matches(pattern: &[u8], value: &[u8], budget: &mut usize) -> Result<bool> {
    let cost = (pattern.len() + 1).saturating_mul(value.len() + 1);
    ensure!(cost <= *budget, "ignore matching budget exceeded");
    *budget -= cost;
    let mut row = vec![false; value.len() + 1];
    row[0] = true;
    let mut i = 0;
    while i < pattern.len() {
        let p = pattern[i];
        let mut next = vec![false; row.len()];
        if p == b'*' {
            let start = i;
            while i + 1 < pattern.len() && pattern[i + 1] == b'*' {
                i += 1;
            }
            let recursive = i > start
                && (start == 0 || pattern[start - 1] == b'/')
                && (i + 1 == pattern.len() || pattern[i + 1] == b'/');
            if recursive && pattern.get(i + 1) == Some(&b'/') {
                // **/ matches zero or more complete path components.
                let mut reachable = false;
                next.clone_from(&row);
                for (j, b) in value.iter().enumerate() {
                    reachable |= row[j];
                    if *b == b'/' && reachable {
                        next[j + 1] = true;
                    }
                }
                i += 1;
            } else {
                next[0] = row[0];
                for (j, b) in value.iter().enumerate() {
                    next[j + 1] = row[j + 1] || next[j] && (recursive || *b != b'/');
                }
            }
        } else {
            let mut literal = p;
            if p == b'\\' && i + 1 < pattern.len() {
                i += 1;
                literal = pattern[i];
            }
            let class = if p == b'[' {
                pattern[i + 1..]
                    .iter()
                    .position(|b| *b == b']')
                    .map(|end| &pattern[i + 1..i + 1 + end])
            } else {
                None
            };
            for (j, b) in value.iter().enumerate() {
                let hit = if let Some(class) = class {
                    let negative = class.first().is_some_and(|b| matches!(b, b'!' | b'^'));
                    let class = if negative { &class[1..] } else { class };
                    let mut found = false;
                    let mut k = 0;
                    while k < class.len() {
                        if k + 2 < class.len() && class[k + 1] == b'-' {
                            found |= (class[k]..=class[k + 2]).contains(b);
                            k += 3;
                        } else {
                            found |= class[k] == *b;
                            k += 1;
                        }
                    }
                    *b != b'/' && found != negative
                } else if p == b'?' {
                    *b != b'/'
                } else {
                    *b == literal
                };
                next[j + 1] = row[j] && hit;
            }
            if let Some(class) = class {
                i += class.len() + 1;
            }
        }
        row = next;
        i += 1;
    }
    Ok(row[value.len()])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wildcards() {
        for (p, v, want) in [
            ("*.o", "x.o", true),
            ("*.o", "dir/x.o", false),
            ("**/x", "x", true),
            ("**/x", "a/b/x", true),
            ("a/**/x", "a/x", true),
            ("a/**/x", "a/b/c/x", true),
            ("a/*/x", "a/b/c/x", false),
            ("a/**", "a/b/c", true),
            ("[a-c]?.txt", "b1.txt", true),
            ("[!a-c]", "b", false),
            ("\\!file", "!file", true),
        ] {
            assert_eq!(
                matches(p.as_bytes(), v.as_bytes(), &mut 100000).unwrap(),
                want,
                "{p} {v}"
            );
        }
    }
}
