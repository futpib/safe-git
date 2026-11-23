//! Git binary patch framing, decoded with bounded zlib and the existing checked delta reader.
use anyhow::{Context, Result, ensure};
use std::io::Write;
const ALPHABET: &[u8] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";
const LIMIT: usize = 16 * 1024 * 1024;
#[derive(Clone)]
pub struct Block {
    delta: bool,
    data: Vec<u8>,
}
impl Block {
    pub fn apply(&self, base: &[u8]) -> Result<Vec<u8>> {
        if self.delta {
            crate::pack::apply_delta(base, &self.data)
        } else {
            Ok(self.data.clone())
        }
    }
}
pub fn parse(lines: &[&[u8]], cursor: &mut usize) -> Result<Block> {
    let header =
        std::str::from_utf8(lines.get(*cursor).context("missing binary patch block")?)?.trim();
    let (kind, size) = header
        .split_once(' ')
        .context("invalid binary patch block")?;
    let size = size.parse::<usize>()?;
    ensure!(
        matches!(kind, "literal" | "delta") && size <= LIMIT,
        "invalid binary patch type or size"
    );
    *cursor += 1;
    let mut compressed = Vec::new();
    while let Some(line) = lines.get(*cursor) {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        if line.is_empty() {
            *cursor += 1;
            break;
        }
        let size = match line[0] {
            b'A'..=b'Z' => (line[0] - b'A' + 1) as usize,
            b'a'..=b'z' => (line[0] - b'a' + 27) as usize,
            _ => anyhow::bail!("invalid binary patch line length"),
        };
        ensure!(
            line.len() == 1 + size.div_ceil(4) * 5,
            "invalid binary patch line"
        );
        let mut bytes = Vec::new();
        for chunk in line[1..].as_chunks::<5>().0 {
            let mut word = 0u32;
            for c in chunk {
                let n = ALPHABET
                    .iter()
                    .position(|v| v == c)
                    .context("invalid binary patch digit")? as u32;
                word = word
                    .checked_mul(85)
                    .and_then(|v| v.checked_add(n))
                    .context("binary patch digit overflow")?;
            }
            bytes.extend(word.to_be_bytes());
        }
        compressed.extend_from_slice(&bytes[..size]);
        ensure!(compressed.len() <= LIMIT, "binary patch exceeds limit");
        *cursor += 1;
    }
    let (data, used) = crate::pack::inflate(&compressed, size)?;
    ensure!(used == compressed.len(), "trailing compressed binary data");
    Ok(Block {
        delta: kind == "delta",
        data,
    })
}
pub fn encode(bytes: &[u8]) -> Result<Vec<u8>> {
    ensure!(bytes.len() <= LIMIT, "binary patch exceeds limit");
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes)?;
    let compressed = encoder.finish()?;
    let mut out = format!("literal {}\n", bytes.len()).into_bytes();
    for line in compressed.chunks(52) {
        out.push(if line.len() <= 26 {
            b'A' + line.len() as u8 - 1
        } else {
            b'a' + line.len() as u8 - 27
        });
        for chunk in line.chunks(4) {
            let mut word = [0; 4];
            word[..chunk.len()].copy_from_slice(chunk);
            let mut value = u32::from_be_bytes(word);
            let mut digits = [0; 5];
            for i in (0..5).rev() {
                digits[i] = ALPHABET[(value % 85) as usize];
                value /= 85;
            }
            out.extend(digits);
        }
        out.push(b'\n');
    }
    out.push(b'\n');
    Ok(out)
}
