use crate::{
    object::{Objects, Oid},
    pack, repo,
    transport::{self, Auth, Remote, Wire},
};
use anyhow::{Context, Result, ensure};
use std::collections::{HashMap, HashSet};

pub struct Advertisement {
    pub refs: HashMap<String, Oid>,
    caps: HashSet<String>,
}
impl Advertisement {
    fn new() -> Self {
        Self {
            refs: HashMap::new(),
            caps: HashSet::new(),
        }
    }
    fn line(&mut self, bytes: &[u8], first: bool) -> Result<()> {
        let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        let line = if let Some(nul) = bytes.iter().position(|b| *b == 0) {
            ensure!(first, "capabilities after first advertisement");
            let caps = std::str::from_utf8(&bytes[nul + 1..])?;
            ensure!(
                caps.is_ascii() && !caps.chars().any(char::is_control),
                "invalid capabilities"
            );
            for cap in caps.split(' ').filter(|c| !c.is_empty()) {
                ensure!(
                    !cap.starts_with("object-format=") || cap == "object-format=sha1",
                    "only SHA-1 repositories are supported"
                );
                self.caps.insert(cap.to_owned());
            }
            &bytes[..nul]
        } else {
            bytes
        };
        ensure!(
            line.len() > 41 && line[40] == b' ',
            "invalid ref advertisement"
        );
        let id = Oid::parse(&line[..40])?;
        let name = std::str::from_utf8(&line[41..])?;
        if id == Oid::ZERO && name == "capabilities^{}" && first {
            return Ok(());
        }
        ensure!(id != Oid::ZERO, "zero object ID in advertised ref");
        if name != "HEAD" {
            repo::refname(name.strip_suffix("^{}").unwrap_or(name))?;
        }
        ensure!(
            self.refs.insert(name.to_owned(), id).is_none(),
            "duplicate advertised ref"
        );
        Ok(())
    }
    pub fn branch(&self, name: &str) -> Option<Oid> {
        self.refs.get(&format!("refs/heads/{name}")).copied()
    }
}
pub async fn advertise(wire: &mut Wire) -> Result<Advertisement> {
    let mut ad = Advertisement::new();
    let mut first = true;
    let mut bytes = 0;
    let mut count = 0;
    while let Some(line) = wire.packet().await? {
        bytes += line.len();
        count += 1;
        ensure!(
            bytes <= 8 * 1024 * 1024 && count <= 100_000,
            "advertisement exceeds limit"
        );
        ad.line(&line, first)?;
        first = false;
    }
    Ok(ad)
}

pub async fn fetch(
    remote: &Remote,
    auth: &Auth,
    branch: &str,
    expected: Option<Oid>,
    haves: &[Oid],
) -> Result<(Oid, Objects)> {
    let mut wire = Wire::connect(remote, auth, false).await?;
    let ad = advertise(&mut wire).await?;
    let tip = ad.branch(branch).context("remote branch does not exist")?;
    ensure!(
        expected.is_none_or(|id| id == tip),
        "remote branch changed while preparing push; retry"
    );
    let mut caps = String::new();
    for capability in ["ofs-delta", "no-progress"] {
        if ad.caps.contains(capability) {
            caps.push(' ');
            caps.push_str(capability);
        }
    }
    let mut request = transport::packet(format!("want {tip}{caps}\n").as_bytes())?;
    request.extend_from_slice(b"0000");
    ensure!(haves.len() <= 32, "too many negotiation tips");
    for have in haves {
        request.extend(transport::packet(format!("have {have}\n").as_bytes())?);
    }
    request.extend(transport::packet(b"done\n")?);
    wire.send(request).await?;
    wire.eof().await?;
    let response = wire
        .packet()
        .await?
        .context("missing negotiation response")?;
    if response != b"NAK\n" {
        let acknowledged = response
            .strip_prefix(b"ACK ")
            .and_then(|r| r.strip_suffix(b"\n"))
            .context("invalid negotiation response")?;
        ensure!(
            haves.contains(&Oid::parse(acknowledged)?),
            "server acknowledged an unoffered commit"
        );
    }
    let bytes = wire.remainder().await?;
    // Git can acknowledge another offered commit after initially acknowledging
    // the wanted tip, even without multi_ack. Only accept our exact offered IDs.
    let mut rest = bytes.as_slice();
    let mut extra = 0;
    while !rest.starts_with(b"PACK") {
        ensure!(
            extra < haves.len() && rest.len() >= 4,
            "missing pack after negotiation"
        );
        extra += 1;
        ensure!(
            rest[..4].iter().all(u8::is_ascii_hexdigit),
            "invalid trailing negotiation packet"
        );
        let size = usize::from_str_radix(std::str::from_utf8(&rest[..4])?, 16)?;
        ensure!(
            (4..=65520).contains(&size) && size <= rest.len(),
            "truncated trailing negotiation packet"
        );
        let id = rest[4..size]
            .strip_prefix(b"ACK ")
            .and_then(|r| r.strip_suffix(b"\n"))
            .context("unexpected trailing negotiation response")?;
        ensure!(
            haves.contains(&Oid::parse(id)?),
            "server acknowledged an unoffered commit"
        );
        rest = &rest[size..];
    }
    Ok((tip, pack::decode(rest)?))
}

pub struct Push {
    wire: Wire,
    pub old: Option<Oid>,
    branch: String,
}
impl Push {
    pub async fn prepare(remote: &Remote, auth: &Auth, branch: &str) -> Result<Self> {
        let mut wire = Wire::connect(remote, auth, true).await?;
        let ad = advertise(&mut wire).await?;
        ensure!(
            ad.caps.contains("report-status"),
            "server does not support required report-status capability"
        );
        Ok(Self {
            old: ad.branch(branch),
            branch: branch.to_owned(),
            wire,
        })
    }
    pub async fn unchanged(mut self) -> Result<()> {
        self.wire.send(b"0000".to_vec()).await?;
        self.wire.eof().await?;
        ensure!(
            self.wire.remainder().await?.is_empty(),
            "unexpected response to empty push"
        );
        Ok(())
    }
    pub async fn send(mut self, new: Oid, pack: Vec<u8>) -> Result<()> {
        ensure!(new != Oid::ZERO, "deletion is unsupported");
        let name = format!("refs/heads/{}", self.branch);
        let mut request = transport::packet(
            format!(
                "{} {new} {name}\0report-status\n",
                self.old.unwrap_or(Oid::ZERO)
            )
            .as_bytes(),
        )?;
        request.extend_from_slice(b"0000");
        self.wire.send(request).await?;
        self.wire.send(pack).await?;
        self.wire.eof().await?;
        let unpack = self.wire.packet().await?.context("missing unpack status")?;
        ensure!(
            unpack == b"unpack ok\n",
            "remote unpack failed: {:?}",
            String::from_utf8_lossy(&unpack)
        );
        let status = self.wire.packet().await?.context("missing ref status")?;
        ensure!(
            status == format!("ok {name}\n").as_bytes(),
            "remote rejected push: {:?}",
            String::from_utf8_lossy(&status)
        );
        ensure!(
            self.wire.packet().await?.is_none(),
            "unexpected extra ref status"
        );
        ensure!(
            self.wire.remainder().await?.is_empty(),
            "unexpected bytes after push status"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn advertisement_validation() {
        let mut ad = Advertisement::new();
        ad.line(
            b"0000000000000000000000000000000000000000 capabilities^{}\0report-status\n",
            true,
        )
        .unwrap();
        assert!(ad.refs.is_empty());
        assert!(ad.caps.contains("report-status"));
        let line = b"1111111111111111111111111111111111111111 refs/heads/main\n";
        ad.line(line, false).unwrap();
        assert!(ad.line(line, false).is_err());
        for bad in [
            &b"garbage"[..],
            &b"1111111111111111111111111111111111111111 refs/heads/../x"[..],
            &b"1111111111111111111111111111111111111111 HEAD\0object-format=sha256"[..],
        ] {
            assert!(Advertisement::new().line(bad, true).is_err());
        }
    }
}
