//! Verify host-owned known_hosts records, including CA and revocation markers.
use anyhow::{Context, Result, ensure};
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use russh::keys::{
    self, PublicKeyOrCertificate,
    ssh_key::{HashAlg, certificate::CertType},
};
use std::{io::Read, path::Path};

fn host_matches(patterns: &str, host: &str) -> Result<bool> {
    let mut matched = false;
    for pattern in patterns.split(',') {
        let (negative, pattern) = pattern
            .strip_prefix('!')
            .map(|p| (true, p))
            .unwrap_or((false, pattern));
        let yes = if let Some(hash) = pattern.strip_prefix("|1|") {
            let (salt, expected) = hash
                .split_once('|')
                .context("invalid hashed known-hosts entry")?;
            let salt = base64::engine::general_purpose::STANDARD.decode(salt)?;
            let expected = base64::engine::general_purpose::STANDARD.decode(expected)?;
            ensure!(
                salt.len() == 20 && expected.len() == 20,
                "invalid known-hosts hash size"
            );
            Hmac::<sha1::Sha1>::new_from_slice(&salt)?
                .chain_update(host.as_bytes())
                .verify_slice(&expected)
                .is_ok()
        } else {
            crate::ssh_config::pattern(pattern, host)
        };
        if negative && yes {
            return Ok(false);
        }
        if !negative {
            matched |= yes;
        }
    }
    Ok(matched)
}
pub fn verify(
    path: &Path,
    host: &str,
    port: u16,
    presented: &PublicKeyOrCertificate,
) -> Result<()> {
    let mut text = String::new();
    std::fs::File::open(path)
        .context("read trusted known_hosts")?
        .take(1024 * 1024 + 1)
        .read_to_string(&mut text)?;
    ensure!(text.len() <= 1024 * 1024, "known_hosts exceeds limit");
    let name = if port == 22 {
        host.to_owned()
    } else {
        format!("[{host}]:{port}")
    };
    verify_text(&text, &name, host, presented)
}
pub fn has_authority(path: &Path, host: &str, port: u16) -> Result<bool> {
    let mut text = String::new();
    std::fs::File::open(path)
        .context("read trusted known_hosts")?
        .take(1024 * 1024 + 1)
        .read_to_string(&mut text)?;
    ensure!(text.len() <= 1024 * 1024, "known_hosts exceeds limit");
    let name = if port == 22 {
        host.to_owned()
    } else {
        format!("[{host}]:{port}")
    };
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() == Some("@cert-authority")
            && host_matches(fields.next().context("missing CA host pattern")?, &name)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}
fn verify_text(
    text: &str,
    name: &str,
    host: &str,
    presented: &PublicKeyOrCertificate,
) -> Result<()> {
    let mut trusted = false;
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let mut fields = line.split_whitespace();
        let first = fields.next().context("invalid known-hosts record")?;
        let (marker, pattern) = if first.starts_with('@') {
            ensure!(
                matches!(first, "@cert-authority" | "@revoked"),
                "unsupported known-hosts marker"
            );
            (first, fields.next().context("missing host pattern")?)
        } else {
            ("", first)
        };
        if !host_matches(pattern, name)? {
            continue;
        }
        let algorithm = fields.next().context("missing host key algorithm")?;
        let key = keys::parse_public_key_base64(fields.next().context("missing host key")?)?;
        ensure!(
            key.algorithm().as_str() == algorithm,
            "known-hosts key type mismatch"
        );
        match presented {
            PublicKeyOrCertificate::PublicKey { key: actual, .. } => {
                if key == *actual {
                    ensure!(marker != "@revoked", "SSH host key is revoked");
                    trusted |= marker.is_empty();
                }
            }
            PublicKeyOrCertificate::Certificate(cert) => {
                if marker == "@revoked" {
                    ensure!(
                        key.key_data() != cert.public_key()
                            && key.key_data() != cert.signature_key(),
                        "SSH host certificate or CA is revoked"
                    );
                } else if marker == "@cert-authority" && key.key_data() == cert.signature_key() {
                    ensure!(
                        cert.cert_type() == CertType::Host,
                        "expected an SSH host certificate"
                    );
                    ensure!(
                        cert.critical_options().is_empty(),
                        "unsupported host certificate critical options"
                    );
                    ensure!(
                        cert.valid_principals().is_empty()
                            || cert.valid_principals().iter().any(|p| p == host),
                        "SSH certificate hostname mismatch"
                    );
                    cert.validate([&key.fingerprint(HashAlg::Sha256)])
                        .context("verify SSH host certificate")?;
                    trusted = true;
                }
            }
        }
    }
    ensure!(
        trusted,
        "SSH host key is not trusted; register its key or CA in the host's known_hosts file"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hashed_and_negated_host_patterns() {
        assert!(host_matches("*.example,!bad.example", "good.example").unwrap());
        assert!(!host_matches("*.example,!bad.example", "bad.example").unwrap());
        let salt = [7u8; 20];
        let digest = Hmac::<sha1::Sha1>::new_from_slice(&salt)
            .unwrap()
            .chain_update(b"[host]:2222")
            .finalize()
            .into_bytes();
        let encode = &base64::engine::general_purpose::STANDARD;
        let pattern = format!("|1|{}|{}", encode.encode(salt), encode.encode(digest));
        assert!(host_matches(&pattern, "[host]:2222").unwrap());
        assert!(!host_matches(&pattern, "[other]:2222").unwrap());
    }
}
