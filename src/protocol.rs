use crate::{
    object::{Objects, Oid},
    pack, repo,
    transport::{self, Auth, Remote, Wire},
};
use anyhow::{Context, Result, ensure};
use std::collections::{HashMap, HashSet};

pub struct Advertisement {
    pub refs: HashMap<String, Oid>,
    pub(crate) caps: HashSet<String>,
}
impl Advertisement {
    pub(crate) fn new() -> Self {
        Self {
            refs: HashMap::new(),
            caps: HashSet::new(),
        }
    }
    pub(crate) fn line(&mut self, bytes: &[u8], first: bool) -> Result<()> {
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
        // receive-pack advertises objects from shared storage / other namespaces
        // as repeatable .have hints. They are not refs and never select a branch.
        // We do not use these optional hints to omit objects from a push.
        if name == ".have" {
            return Ok(());
        }
        if name != "HEAD" {
            repo::refname(name.strip_suffix("^{}").unwrap_or(name))
                .with_context(|| format!("invalid advertised ref {name}"))?;
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
    pub fn clone_branch(&self, requested: Option<&str>) -> Result<(String, Option<Oid>)> {
        if let Some(name) = requested {
            let name = repo::branch(name)?;
            let tip = self.branch(&name).context("remote branch does not exist")?;
            return Ok((name, Some(tip)));
        }
        let mut targets = self
            .caps
            .iter()
            .filter_map(|cap| cap.strip_prefix("symref=HEAD:"));
        let target = targets.next();
        ensure!(targets.next().is_none(), "conflicting remote HEAD targets");
        if let Some(target) = target {
            let name = repo::branch(
                target
                    .strip_prefix("refs/heads/")
                    .context("remote HEAD must name a branch")?,
            )?;
            let tip = self.branch(&name);
            ensure!(
                tip.is_some() || self.refs.is_empty(),
                "remote HEAD names a missing branch"
            );
            ensure!(
                self.refs.get("HEAD").is_none_or(|id| Some(*id) == tip),
                "remote HEAD disagrees with its branch"
            );
            return Ok((name, tip));
        }
        if self.refs.is_empty() {
            // Protocol v0 cannot advertise the unborn HEAD of an empty repo.
            return Ok(("master".to_owned(), None));
        }
        let head = self
            .refs
            .get("HEAD")
            .context("remote HEAD is missing; select --branch")?;
        let mut names = self.refs.iter().filter_map(|(name, id)| {
            (id == head)
                .then(|| name.strip_prefix("refs/heads/"))
                .flatten()
        });
        let name = names
            .next()
            .context("remote HEAD is detached; select --branch")?;
        ensure!(
            names.next().is_none(),
            "remote HEAD is ambiguous; select --branch"
        );
        Ok((repo::branch(name)?, Some(*head)))
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

pub(crate) fn request(ad: &Advertisement, tips: &[Oid], haves: &[Oid]) -> Result<Vec<u8>> {
    ensure!(
        !tips.is_empty() && tips.len() <= 100_000,
        "invalid want count"
    );
    request_depth(ad, tips, haves, None)
}
pub(crate) fn request_depth(
    ad: &Advertisement,
    tips: &[Oid],
    haves: &[Oid],
    depth: Option<u32>,
) -> Result<Vec<u8>> {
    ensure!(
        !tips.is_empty() && tips.len() <= 100_000,
        "invalid want count"
    );
    let mut caps = String::new();
    for capability in ["ofs-delta", "no-progress"] {
        if ad.caps.contains(capability) {
            caps.push(' ');
            caps.push_str(capability);
        }
    }
    let mut request = Vec::new();
    for (i, tip) in tips.iter().enumerate() {
        let caps = if i == 0 { caps.as_str() } else { "" };
        request.extend(transport::packet(format!("want {tip}{caps}\n").as_bytes())?);
    }
    if let Some(depth) = depth {
        ensure!(
            depth > 0 && ad.caps.contains("shallow"),
            "server does not support shallow fetch"
        );
        request.extend(transport::packet(format!("deepen {depth}\n").as_bytes())?);
    }
    request.extend_from_slice(b"0000");
    ensure!(haves.len() <= 32, "too many negotiation tips");
    for have in haves {
        request.extend(transport::packet(format!("have {have}\n").as_bytes())?);
    }
    request.extend(transport::packet(b"done\n")?);
    Ok(request)
}
async fn transfer(
    mut wire: Wire,
    ad: &Advertisement,
    tips: &[Oid],
    haves: &[Oid],
) -> Result<Objects> {
    wire.send(request(ad, tips, haves)?).await?;
    wire.eof().await?;
    let response = wire
        .packet()
        .await?
        .context("missing negotiation response")?;
    check_ack(&response, haves)?;
    decode_pack(&wire.remainder().await?, haves)
}
pub(crate) fn check_ack(response: &[u8], haves: &[Oid]) -> Result<()> {
    if response != b"NAK\n" {
        let id = response
            .strip_prefix(b"ACK ")
            .and_then(|r| r.strip_suffix(b"\n"))
            .context("invalid negotiation response")?;
        ensure!(
            haves.contains(&Oid::parse(id)?),
            "server acknowledged an unoffered commit"
        );
    }
    Ok(())
}
pub async fn fetch_many(
    remote: &Remote,
    auth: &Auth,
    refs: &HashMap<String, Oid>,
    haves: &[Oid],
) -> Result<Objects> {
    let mut wire = Wire::connect(remote, auth, false).await?;
    let ad = advertise(&mut wire).await?;
    for (name, id) in refs {
        ensure!(
            ad.refs.get(name) == Some(id),
            "remote ref changed while fetching; retry"
        );
    }
    let tips: HashSet<_> = refs.values().copied().collect();
    transfer(wire, &ad, &tips.into_iter().collect::<Vec<_>>(), haves).await
}

pub(crate) fn decode_pack(bytes: &[u8], haves: &[Oid]) -> Result<Objects> {
    // Git can acknowledge another offered commit after initially acknowledging
    // the wanted tip, even without multi_ack. Only accept our exact offered IDs.
    let mut rest = bytes;
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
    pack::decode(rest)
}

/// Read refs without requesting objects or running a local helper.
pub async fn discover(remote: &Remote, auth: &Auth) -> Result<Advertisement> {
    let mut wire = Wire::connect(remote, auth, false).await?;
    let ad = advertise(&mut wire).await?;
    wire.send(b"0000".to_vec()).await?;
    wire.eof().await?;
    ensure!(
        wire.remainder().await?.is_empty(),
        "unexpected response to ref discovery"
    );
    Ok(ad)
}

pub struct Push {
    wire: Wire,
    pub old: Option<Oid>,
    name: String,
    delete_refs: bool,
}
impl Push {
    pub async fn prepare(remote: &Remote, auth: &Auth, name: &str) -> Result<Self> {
        repo::refname(name)?;
        let mut wire = Wire::connect(remote, auth, true).await?;
        let ad = advertise(&mut wire).await?;
        ensure!(
            ad.caps.contains("report-status"),
            "server does not support required report-status capability"
        );
        Ok(Self {
            old: ad.refs.get(name).copied(),
            name: name.to_owned(),
            delete_refs: ad.caps.contains("delete-refs"),
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
        ensure!(
            new != Oid::ZERO || self.delete_refs,
            "server does not allow deletion"
        );
        let name = self.name.clone();
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
        // The flush terminates report-status, as in Git's receive_status().
        // Later server output cannot change the acknowledged result. Drain it
        // under the transport bounds without displaying or interpreting it,
        // and still require a successful SSH service exit.
        self.wire.remainder().await?;
        Ok(())
    }
}

pub async fn fetch_depth(
    remote: &Remote,
    auth: &Auth,
    refs: &HashMap<String, Oid>,
    depth: u32,
) -> Result<crate::network::Fetched> {
    let mut wire = Wire::connect(remote, auth, false).await?;
    let ad = advertise(&mut wire).await?;
    for (name, id) in refs {
        ensure!(
            ad.refs.get(name) == Some(id),
            "remote ref changed while fetching; retry"
        );
    }
    let tips: Vec<_> = refs
        .values()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    wire.send(request_depth(&ad, &tips, &[], Some(depth))?)
        .await?;
    wire.eof().await?;
    let mut shallow = HashSet::new();
    while let Some(line) = wire.packet().await? {
        shallow_line(&line, &mut shallow)?;
    }
    check_ack(
        &wire
            .packet()
            .await?
            .context("missing negotiation response")?,
        &[],
    )?;
    Ok(crate::network::Fetched {
        objects: decode_pack(&wire.remainder().await?, &[])?,
        shallow,
    })
}
pub(crate) fn shallow_line(line: &[u8], shallow: &mut HashSet<Oid>) -> Result<()> {
    let value = line
        .strip_prefix(b"shallow ")
        .map(|v| v.strip_suffix(b"\n").unwrap_or(v))
        .with_context(|| {
            format!(
                "invalid shallow response: {:?}",
                String::from_utf8_lossy(line)
            )
        })?;
    ensure!(
        shallow.len() < crate::object::MAX_OBJECTS && shallow.insert(Oid::parse(value)?),
        "duplicate or excessive shallow boundaries"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shallow_boundaries_are_bounded_data_with_optional_newlines() {
        for suffix in ["", "\n"] {
            let mut ids = HashSet::new();
            let line = format!("shallow 1111111111111111111111111111111111111111{suffix}");
            shallow_line(line.as_bytes(), &mut ids).unwrap();
            assert!(shallow_line(line.as_bytes(), &mut ids).is_err());
        }
        for bad in [
            "unshallow 1111111111111111111111111111111111111111",
            "shallow ../outside",
            "shallow 1111111111111111111111111111111111111111\nexec x",
        ] {
            assert!(shallow_line(bad.as_bytes(), &mut HashSet::new()).is_err());
        }
    }
    #[test]
    fn clone_head_selection_validates_symrefs_and_ambiguity() {
        let mut ad = Advertisement::new();
        assert_eq!(ad.clone_branch(None).unwrap(), ("master".into(), None));
        assert!(ad.clone_branch(Some("missing")).is_err());
        ad.line(
            b"1111111111111111111111111111111111111111 HEAD\0symref=HEAD:refs/heads/master\n",
            true,
        )
        .unwrap();
        assert!(ad.clone_branch(None).is_err());
        for name in ["master", "other"] {
            ad.line(
                format!("1111111111111111111111111111111111111111 refs/heads/{name}\n").as_bytes(),
                false,
            )
            .unwrap();
        }
        assert_eq!(ad.clone_branch(None).unwrap().0, "master");
        assert_eq!(ad.clone_branch(Some("other")).unwrap().0, "other");
        ad.caps.insert("symref=HEAD:refs/heads/other".into());
        assert!(ad.clone_branch(None).is_err());
        ad.caps.clear();
        assert!(ad.clone_branch(None).is_err());
        ad.refs.remove("refs/heads/other");
        assert_eq!(ad.clone_branch(None).unwrap().0, "master");
        for target in [
            "refs/tags/tag",
            "refs/heads/../escape",
            "refs/heads/missing",
        ] {
            ad.caps.clear();
            ad.caps.insert(format!("symref=HEAD:{target}"));
            assert!(ad.clone_branch(None).is_err());
        }
        ad.caps.clear();
        ad.caps.insert("symref=HEAD:refs/heads/master".into());
        ad.refs.insert(
            "HEAD".into(),
            Oid::parse(b"2222222222222222222222222222222222222222").unwrap(),
        );
        assert!(ad.clone_branch(None).is_err());
    }
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

    #[test]
    fn shared_object_hints_are_not_refs() {
        let mut ad = Advertisement::new();
        ad.line(
            b"1111111111111111111111111111111111111111 .have\0report-status\n",
            true,
        )
        .unwrap();
        ad.line(b"2222222222222222222222222222222222222222 .have\n", false)
            .unwrap();
        ad.line(b"1111111111111111111111111111111111111111 .have\n", false)
            .unwrap();
        assert!(ad.refs.is_empty());
        assert!(ad.caps.contains("report-status"));
        assert!(ad.branch(".have").is_none());
        ad.line(
            b"3333333333333333333333333333333333333333 refs/heads/main\n",
            false,
        )
        .unwrap();
        assert_eq!(ad.refs.len(), 1);
        assert_eq!(
            ad.branch("main"),
            Some(Oid::parse(b"3333333333333333333333333333333333333333").unwrap())
        );
        for bad in [
            &b"0000000000000000000000000000000000000000 .have\n"[..],
            &b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx .have\n"[..],
            &b"1111111111111111111111111111111111111111 .have/../main\n"[..],
            &b"1111111111111111111111111111111111111111 .have^{}\n"[..],
            &b"1111111111111111111111111111111111111111 .have\0report-status\n"[..],
        ] {
            assert!(Advertisement::new().line(bad, false).is_err());
        }
    }
}
