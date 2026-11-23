//! Bounded MIME decoding. Attachment names and all routing headers remain inert.
use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use std::collections::BTreeMap;
const LIMIT: usize = 16 * 1024 * 1024;
pub fn headers(text: &str) -> Result<BTreeMap<String, String>> {
    ensure!(text.len() <= 1024 * 1024, "mail headers exceed limit");
    let mut fields = BTreeMap::<String, String>::new();
    let mut last = String::new();
    for line in text.lines() {
        if line.starts_with([' ', '\t']) {
            let value = fields.get_mut(&last).context("bad folded header")?;
            value.push(' ');
            value.push_str(line.trim());
        } else {
            let (key, value) = line.split_once(':').context("bad email header")?;
            ensure!(
                key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                "invalid header name"
            );
            last = key.to_ascii_lowercase();
            ensure!(
                fields.insert(last.clone(), value.trim().into()).is_none(),
                "duplicate mail header"
            );
        }
    }
    Ok(fields)
}
fn qp(bytes: &[u8], header: bool) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'=' if bytes.get(i + 1) == Some(&b'\n') => i += 2,
            b'=' if bytes.get(i + 1..i + 3) == Some(b"\r\n") => i += 3,
            b'=' => {
                let pair = bytes
                    .get(i + 1..i + 3)
                    .context("truncated quoted printable escape")?;
                let mut byte = [0];
                hex::decode_to_slice(pair, &mut byte).context("invalid quoted printable escape")?;
                out.push(byte[0]);
                i += 3;
            }
            b'_' if header => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    Ok(out)
}
fn charset(bytes: &[u8], label: &str) -> Result<String> {
    let encoding =
        encoding_rs::Encoding::for_label(label.as_bytes()).context("unknown mail charset")?;
    let (text, _, errors) = encoding.decode(bytes);
    ensure!(!errors, "invalid encoded mail text");
    ensure!(text.len() <= LIMIT, "decoded mail exceeds limit");
    Ok(text.into_owned())
}
pub fn header(text: &str) -> Result<String> {
    let mut out = String::new();
    let mut rest = text;
    let mut previous = false;
    while let Some(at) = rest.find("=?") {
        let prefix = &rest[..at];
        if !previous || !prefix.trim().is_empty() {
            out.push_str(prefix);
        }
        let encoded = &rest[at + 2..];
        let (label, encoded) = encoded
            .split_once('?')
            .context("bad encoded header charset")?;
        let (kind, encoded) = encoded
            .split_once('?')
            .context("bad encoded header encoding")?;
        let (data, tail) = encoded
            .split_once("?=")
            .context("unterminated encoded header")?;
        let bytes = match kind.to_ascii_lowercase().as_str() {
            "b" => base64::engine::general_purpose::STANDARD.decode(data)?,
            "q" => qp(data.as_bytes(), true)?,
            _ => bail!("invalid encoded header encoding"),
        };
        out.push_str(&charset(&bytes, label)?);
        ensure!(out.len() <= 1024 * 1024, "decoded header exceeds limit");
        rest = tail;
        previous = true;
    }
    out.push_str(rest);
    ensure!(
        !out.chars().any(char::is_control),
        "control characters in decoded header"
    );
    Ok(out)
}
fn parameter(value: &str, key: &str) -> Option<String> {
    value.split(';').skip(1).find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        k.eq_ignore_ascii_case(key)
            .then(|| v.trim().trim_matches('"').to_owned())
    })
}
pub fn body(fields: &BTreeMap<String, String>, body: &str) -> Result<String> {
    body_inner(fields, body, 0, &mut 1000usize)
}
fn body_inner(
    fields: &BTreeMap<String, String>,
    body: &str,
    depth: usize,
    parts: &mut usize,
) -> Result<String> {
    ensure!(
        depth <= 16 && *parts > 0 && body.len() <= LIMIT,
        "MIME nesting or size exceeds limit"
    );
    *parts -= 1;
    let content_type = fields
        .get("content-type")
        .map(String::as_str)
        .unwrap_or("text/plain; charset=utf-8");
    if content_type.to_ascii_lowercase().starts_with("multipart/") {
        let boundary = parameter(content_type, "boundary").context("multipart boundary missing")?;
        ensure!(
            !boundary.is_empty()
                && boundary.len() <= 200
                && !boundary.chars().any(char::is_control),
            "invalid MIME boundary"
        );
        let marker = format!("--{boundary}");
        let close = format!("{marker}--");
        let mut out = String::new();
        let mut part = String::new();
        let mut active = false;
        let mut closed = false;
        for line in body.split_inclusive('\n') {
            if matches!(line.trim_end_matches(['\n', '\r']), v if v == marker || v == close) {
                if active {
                    let (head, body) = part
                        .split_once("\n\n")
                        .context("missing MIME part headers")?;
                    out.push_str(&body_inner(&headers(head)?, body, depth + 1, parts)?);
                    part.clear();
                    ensure!(out.len() <= LIMIT, "decoded MIME exceeds limit");
                }
                if line.trim_end_matches(['\n', '\r']) == close {
                    closed = true;
                    break;
                }
                active = true;
            } else if active {
                part.push_str(line);
            }
        }
        ensure!(closed, "unterminated multipart message");
        return Ok(out);
    }
    let kind = content_type
        .split(';')
        .next()
        .unwrap()
        .trim()
        .to_ascii_lowercase();
    if !matches!(
        kind.as_str(),
        "text/plain"
            | "text/x-patch"
            | "text/x-diff"
            | "application/x-patch"
            | "application/octet-stream"
    ) {
        return Ok(String::new());
    }
    let encoding = fields
        .get("content-transfer-encoding")
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or("8bit".into());
    let bytes = match encoding.as_str() {
        "7bit" | "8bit" | "binary" => body.as_bytes().to_vec(),
        "quoted-printable" => qp(body.as_bytes(), false)?,
        "base64" => {
            let compact: Vec<_> = body.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
            base64::engine::general_purpose::STANDARD.decode(compact)?
        }
        _ => bail!("unsupported mail transfer encoding"),
    };
    charset(
        &bytes,
        &parameter(content_type, "charset").unwrap_or("utf-8".into()),
    )
}
