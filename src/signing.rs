//! SSH signatures using only an explicitly selected host key, entirely in process.
use crate::{
    Cli,
    object::{Kind, Object, Oid, ReadObjects},
    repo::{Repo, Store},
};
use anyhow::{Context, Result, ensure};
pub fn key(cli: &Cli, path: &str) -> Result<russh::keys::PrivateKey> {
    let bytes = crate::mail::read_input(cli, path)?;
    let key =
        russh::keys::PrivateKey::from_openssh(&bytes).context("invalid OpenSSH signing key")?;
    if key.is_encrypted() {
        let password = std::env::var("SAFE_GIT_SIGNING_PASSPHRASE")
            .context("encrypted signing key requires SAFE_GIT_SIGNING_PASSPHRASE")?;
        Ok(key
            .decrypt(password.as_bytes())
            .context("could not decrypt signing key")?)
    } else {
        Ok(key)
    }
}
pub fn commit(
    repo: &Repo,
    store: &mut Store<'_>,
    id: Oid,
    key: &russh::keys::PrivateKey,
) -> Result<Oid> {
    let object = store.get(id)?;
    ensure!(object.kind == Kind::Commit, "signing requires a commit");
    let signature = key
        .sign("git", russh::keys::HashAlg::Sha512, &object.data)?
        .to_pem(russh::keys::ssh_key::LineEnding::LF)?;
    let end = object
        .data
        .windows(2)
        .position(|w| w == b"\n\n")
        .context("invalid commit")?;
    let mut data = object.data[..end + 1].to_vec();
    for (i, line) in signature.lines().enumerate() {
        data.extend_from_slice(if i != 0 {
            b" "
        } else if repo.format == crate::object::Format::Sha256 {
            b"gpgsig-sha256 "
        } else {
            b"gpgsig "
        });
        data.extend_from_slice(line.as_bytes());
        data.push(b'\n');
    }
    data.extend_from_slice(&object.data[end + 1..]);
    let signed = Object {
        kind: Kind::Commit,
        data,
    };
    repo.write_object(&signed)?;
    signed.oid_for(repo.format)
}

pub fn sign_tag(data: &mut Vec<u8>, key: &russh::keys::PrivateKey) -> Result<()> {
    let signature = key
        .sign("git", russh::keys::HashAlg::Sha512, data)?
        .to_pem(russh::keys::ssh_key::LineEnding::LF)?;
    ensure!(
        data.len() + signature.len() <= crate::object::MAX_OBJECT,
        "signed tag exceeds limit"
    );
    data.extend_from_slice(signature.as_bytes());
    Ok(())
}

pub fn verify(
    cli: &Cli,
    repo: &Repo,
    store: &mut Store<'_>,
    command: &str,
    args: &[String],
) -> Result<()> {
    let path = cli.verification_key.as_deref().context("provide --verification-key or SAFE_GIT_VERIFICATION_KEY; repository trust settings are ignored")?;
    let bytes = crate::mail::read_input(cli, path)?;
    let key = russh::keys::PublicKey::from_openssh(std::str::from_utf8(&bytes)?)?;
    ensure!(!args.is_empty(), "provide objects to verify");
    for arg in args {
        ensure!(!arg.starts_with('-'), "unsupported verification option");
        let id = crate::revision::resolve(repo, store, arg)?;
        let obj = store.get(id)?;
        let (payload, pem) = if command == "verify-tag" {
            ensure!(obj.kind == Kind::Tag, "expected annotated tag");
            let marker = b"-----BEGIN SSH SIGNATURE-----\n";
            let at = obj
                .data
                .windows(marker.len())
                .position(|w| w == marker)
                .context("no SSH tag signature")?;
            (obj.data[..at].to_vec(), obj.data[at..].to_vec())
        } else {
            ensure!(obj.kind == Kind::Commit, "expected commit");
            let mut payload = Vec::new();
            let mut pem = Vec::new();
            let mut signature = false;
            let mut headers = true;
            let field = if repo.format == crate::object::Format::Sha256 {
                b"gpgsig-sha256 ".as_slice()
            } else {
                b"gpgsig ".as_slice()
            };
            for line in obj.data.split_inclusive(|b| *b == b'\n') {
                if headers && let Some(first) = line.strip_prefix(field) {
                    ensure!(pem.is_empty(), "duplicate commit signature");
                    signature = true;
                    pem.extend_from_slice(first);
                } else if headers && signature && line.starts_with(b" ") {
                    pem.extend_from_slice(&line[1..]);
                } else {
                    signature = false;
                    if line == b"\n" {
                        headers = false;
                    }
                    payload.extend_from_slice(line);
                }
            }
            ensure!(!pem.is_empty(), "no SSH commit signature");
            (payload, pem)
        };
        let signature = russh::keys::ssh_key::SshSig::from_pem(&pem)?;
        key.verify("git", &payload, &signature)
            .context("signature does not match the caller's trusted key")?;
        println!("Good SSH signature for {id}");
    }
    Ok(())
}
