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
        data.extend_from_slice(if i == 0 { b"gpgsig " } else { b" " });
        data.extend_from_slice(line.as_bytes());
        data.push(b'\n');
    }
    data.extend_from_slice(&object.data[end + 1..]);
    let signed = Object {
        kind: Kind::Commit,
        data,
    };
    repo.write_object(&signed)?;
    signed.oid()
}
