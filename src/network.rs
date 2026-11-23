//! In-process transports. No Git helper, credential helper, URL rewrite, or local subprocess.
use crate::{
    object::{MAX_OBJECTS, MAX_PACK, MAX_TOTAL, Objects, Oid, ReadObjects},
    protocol::{self, Advertisement},
    repo::{Repo, Store},
    transport::{Auth, Remote},
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

enum Endpoint {
    Ssh(Remote),
    Http(url::Url),
    Git(url::Url),
    Local(PathBuf),
}
fn endpoint(value: &str) -> Result<Endpoint> {
    ensure!(
        !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control),
        "invalid repository URL"
    );
    if value.starts_with("https://") || value.starts_with("http://") {
        let url = url::Url::parse(value)?;
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
                && url.query().is_none(),
            "HTTP URL credentials, query and fragment are unsupported"
        );
        return Ok(Endpoint::Http(url));
    }
    if value.starts_with("git://") {
        let url = url::Url::parse(value)?;
        ensure!(
            url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && !value.contains('%'),
            "invalid native Git URL"
        );
        return Ok(Endpoint::Git(url));
    }
    if value.starts_with("file://") {
        let url = url::Url::parse(value)?;
        ensure!(
            url.host_str().is_none_or(|h| h == "localhost")
                && url.query().is_none()
                && url.fragment().is_none(),
            "invalid local repository URL"
        );
        return Ok(Endpoint::Local(
            url.to_file_path()
                .map_err(|_| anyhow::anyhow!("invalid file URL"))?,
        ));
    }
    if value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || !value.contains(':')
    {
        return Ok(Endpoint::Local(PathBuf::from(value)));
    }
    Ok(Endpoint::Ssh(Remote::parse(value)?))
}
pub fn normalize(value: &str) -> Result<String> {
    normalize_from(value, std::path::Path::new("."))
}
pub fn normalize_from(value: &str, directory: &std::path::Path) -> Result<String> {
    match endpoint(value)? {
        Endpoint::Local(path) => Ok(directory
            .join(path)
            .canonicalize()?
            .to_str()
            .context("non-UTF-8 source path")?
            .to_owned()),
        _ => Ok(value.to_owned()),
    }
}
pub(crate) fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(120))
        .user_agent(concat!("safe-git/", env!("CARGO_PKG_VERSION")))
        .build()?)
}
pub(crate) fn service_url(base: &url::Url, suffix: &str) -> Result<url::Url> {
    Ok(url::Url::parse(&format!(
        "{}/{suffix}",
        base.as_str().trim_end_matches('/')
    ))?)
}
async fn body(response: reqwest::Response, kind: &str, limit: usize) -> Result<Vec<u8>> {
    ensure!(
        response.status().is_success(),
        "HTTP Git service returned {} (redirects and authentication prompts are not followed)",
        response.status()
    );
    ensure!(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(';').next() == Some(kind)),
        "unexpected HTTP Git content type"
    );
    read_body(response, limit).await
}
pub(crate) async fn read_body(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    ensure!(
        response.status().is_success(),
        "HTTP Git request failed (redirects are not followed)"
    );
    ensure!(
        response.content_length().is_none_or(|v| v <= limit as u64),
        "HTTP response exceeds limit"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            chunk.len() <= limit - bytes.len(),
            "HTTP response exceeds limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
fn packet<'a>(bytes: &mut &'a [u8]) -> Result<Option<&'a [u8]>> {
    ensure!(
        bytes.len() >= 4 && bytes[..4].iter().all(u8::is_ascii_hexdigit),
        "invalid HTTP pkt-line"
    );
    let size = usize::from_str_radix(std::str::from_utf8(&bytes[..4])?, 16)?;
    if size == 0 {
        *bytes = &bytes[4..];
        return Ok(None);
    }
    ensure!(
        (4..=65520).contains(&size) && size <= bytes.len(),
        "invalid HTTP packet length"
    );
    let value = &bytes[4..size];
    *bytes = &bytes[size..];
    Ok(Some(value))
}
async fn http_ad(client: &reqwest::Client, base: &url::Url) -> Result<(Advertisement, bool)> {
    let mut url = service_url(base, "info/refs")?;
    url.set_query(Some("service=git-upload-pack"));
    let response = http_auth(client.get(url), base)?.send().await?;
    let smart = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';').next() == Some("application/x-git-upload-pack-advertisement")
        });
    if !smart {
        ensure!(
            response.status().is_success(),
            "HTTP Git service returned {} (redirects and authentication prompts are not followed)",
            response.status()
        );
        let bytes = read_body(response, 8 * 1024 * 1024).await?;
        return Ok((
            crate::static_http::advertisement(client, base, &bytes).await?,
            false,
        ));
    }
    let data = body(
        response,
        "application/x-git-upload-pack-advertisement",
        8 * 1024 * 1024,
    )
    .await?;
    let mut bytes = data.as_slice();
    ensure!(
        packet(&mut bytes)? == Some(b"# service=git-upload-pack\n".as_slice()),
        "not a smart HTTP Git service"
    );
    ensure!(packet(&mut bytes)?.is_none(), "missing service flush");
    let mut ad = Advertisement::new();
    let mut count = 0;
    while let Some(line) = packet(&mut bytes)? {
        ensure!(count < 100_000, "too many advertised refs");
        ad.line(line, count == 0)?;
        count += 1;
    }
    ensure!(bytes.is_empty(), "trailing HTTP advertisement data");
    Ok((ad, true))
}
fn local_ad(repo: &Repo) -> Result<Advertisement> {
    let refs = repo.refs()?;
    let mut ad = Advertisement::new();
    if let Some(id) = repo.read_ref("HEAD")? {
        ad.line(
            format!("{id} HEAD\0symref=HEAD:{}\n", repo.head()?).as_bytes(),
            true,
        )?;
    }
    for (name, id) in refs {
        ad.line(format!("{id} {name}\n").as_bytes(), false)?;
    }
    // Advertise peeled tag IDs as Git does.
    let mut store = Store::new(repo);
    let tags: Vec<_> = ad
        .refs
        .iter()
        .filter(|(n, _)| n.starts_with("refs/tags/"))
        .map(|(n, id)| (n.clone(), *id))
        .collect();
    for (name, id) in tags {
        let peeled = crate::revision::peel(&mut store, id)?;
        if peeled != id {
            ad.refs.insert(format!("{name}^{{}}"), peeled);
        }
    }
    Ok(ad)
}
pub async fn discover(url: &str, auth: &Auth) -> Result<Advertisement> {
    match endpoint(url)? {
        Endpoint::Ssh(remote) => protocol::discover(&remote, auth).await,
        Endpoint::Http(base) => Ok(http_ad(&client()?, &base).await?.0),
        Endpoint::Git(base) => {
            let (mut socket, ad) = native_ad(&base, false).await?;
            use tokio::io::AsyncWriteExt;
            socket.write_all(b"0000").await?;
            socket.shutdown().await?;
            Ok(ad)
        }
        Endpoint::Local(path) => local_ad(&Repo::open(&path)?),
    }
}
pub async fn fetch(
    url: &str,
    auth: &Auth,
    refs: &HashMap<String, Oid>,
    haves: &[Oid],
) -> Result<Objects> {
    Ok(fetch_depth(url, auth, refs, haves, None).await?.objects)
}
#[derive(Default)]
pub struct FetchOptions {
    pub depth: Option<u32>,
    pub deepen: Option<u32>,
    pub shallow: HashSet<Oid>,
    pub filter: Option<String>,
    pub raw: bool,
}
impl FetchOptions {
    pub fn changes_depth(&self) -> bool {
        self.depth.is_some() || self.deepen.is_some()
    }
}
pub struct Fetched {
    pub objects: Objects,
    pub shallow: HashSet<Oid>,
}
pub async fn fetch_depth(
    url: &str,
    auth: &Auth,
    refs: &HashMap<String, Oid>,
    haves: &[Oid],
    depth: Option<u32>,
) -> Result<Fetched> {
    fetch_options(
        url,
        auth,
        refs,
        haves,
        &FetchOptions {
            depth,
            ..Default::default()
        },
    )
    .await
}
pub async fn fetch_options(
    url: &str,
    auth: &Auth,
    refs: &HashMap<String, Oid>,
    haves: &[Oid],
    options: &FetchOptions,
) -> Result<Fetched> {
    if let Some(filter) = &options.filter {
        crate::partial::filter(filter)?;
    }
    let haves =
        if options.changes_depth() || options.filter.is_some() || !options.shallow.is_empty() {
            &[][..]
        } else {
            haves
        };
    if refs.is_empty() {
        return Ok(Fetched {
            objects: Objects::new(),
            shallow: HashSet::new(),
        });
    }
    match endpoint(url)? {
        Endpoint::Ssh(remote) => protocol::fetch_options(&remote, auth, refs, haves, options).await,
        Endpoint::Http(base) => {
            let client = client()?;
            let (ad, smart) = http_ad(&client, &base).await?;
            for (name, id) in refs {
                ensure!(
                    options.raw || ad.refs.get(name) == Some(id),
                    "remote ref changed while fetching; retry"
                );
            }
            if !smart {
                return crate::static_http::fetch(&client, &base, refs, options).await;
            }
            let tips: HashSet<_> = refs.values().copied().collect();
            let request = protocol::request_options(
                &ad,
                &tips.into_iter().collect::<Vec<_>>(),
                haves,
                options,
            )?;
            let response = http_auth(client.post(service_url(&base, "git-upload-pack")?), &base)?
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/x-git-upload-pack-request",
                )
                .header(
                    reqwest::header::ACCEPT,
                    "application/x-git-upload-pack-result",
                )
                .body(request)
                .send()
                .await?;
            let data = body(response, "application/x-git-upload-pack-result", MAX_PACK).await?;
            let mut bytes = data.as_slice();
            let mut shallow = options.shallow.clone();
            if options.changes_depth() {
                while let Some(line) = packet(&mut bytes)? {
                    protocol::boundary_line(line, &mut shallow, &options.shallow)?;
                }
            }
            protocol::check_ack(
                packet(&mut bytes)?.context("missing negotiation response")?,
                haves,
            )?;
            Ok(Fetched {
                objects: protocol::decode_pack(bytes, haves)?,
                shallow,
            })
        }
        Endpoint::Git(base) => {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, ad) = native_ad(&base, false).await?;
            for (name, id) in refs {
                ensure!(
                    options.raw || ad.refs.get(name) == Some(id),
                    "remote ref changed while fetching; retry"
                );
            }
            let tips: HashSet<_> = refs.values().copied().collect();
            socket
                .write_all(&protocol::request_options(
                    &ad,
                    &tips.into_iter().collect::<Vec<_>>(),
                    haves,
                    options,
                )?)
                .await?;
            socket.shutdown().await?;
            let mut data = Vec::new();
            socket
                .take(MAX_PACK as u64 + 1)
                .read_to_end(&mut data)
                .await?;
            ensure!(data.len() <= MAX_PACK, "native Git response exceeds limit");
            let mut bytes = data.as_slice();
            let mut shallow = options.shallow.clone();
            if options.changes_depth() {
                while let Some(line) = packet(&mut bytes)? {
                    protocol::boundary_line(line, &mut shallow, &options.shallow)?;
                }
            }
            protocol::check_ack(
                packet(&mut bytes)?.context("missing negotiation response")?,
                haves,
            )?;
            Ok(Fetched {
                objects: protocol::decode_pack(bytes, haves)?,
                shallow,
            })
        }
        Endpoint::Local(path) => {
            let repo = Repo::open(&path)?;
            let ad = local_ad(&repo)?;
            let mut store = Store::new(&repo);
            store.allow_promisor = false;
            let mut objects = Objects::new();
            let mut total = 0;
            for (name, id) in refs {
                ensure!(
                    options.raw || ad.refs.get(name) == Some(id),
                    "local source ref changed while fetching; retry"
                );
            }
            let mut shallow = store.shallow.clone();
            if let Some(depth) = options.depth {
                let tips: Vec<_> = refs
                    .values()
                    .map(|id| crate::revision::peel(&mut store, *id))
                    .collect::<Result<_>>()?;
                let commits: Vec<_> = tips
                    .into_iter()
                    .filter(|id| {
                        store
                            .get(*id)
                            .is_ok_and(|o| o.kind == crate::object::Kind::Commit)
                    })
                    .collect();
                shallow.extend(crate::shallow::boundary(&mut store, &commits, depth)?);
                store.shallow = shallow.clone();
            }
            if let Some(deepen) = options.deepen {
                let tips: Vec<_> = options.shallow.iter().copied().collect();
                shallow.extend(crate::shallow::boundary(
                    &mut store,
                    &tips,
                    deepen.checked_add(1).context("depth overflow")?,
                )?);
                store.shallow = shallow.clone();
            }
            let ids = crate::object::reachable_many_since(
                &mut store,
                &refs.values().copied().collect::<Vec<_>>(),
                &HashSet::new(),
            )?;
            for id in ids {
                let obj = store.get(id)?;
                total += obj.data.len();
                ensure!(
                    total <= MAX_TOTAL && objects.len() < MAX_OBJECTS,
                    "local transfer budget exceeded"
                );
                if options
                    .filter
                    .as_deref()
                    .map(|f| crate::partial::omitted(f, &obj))
                    .transpose()?
                    .unwrap_or(false)
                {
                    continue;
                }
                objects.insert(id, obj);
            }
            Ok(Fetched { objects, shallow })
        }
    }
}

async fn native_ad(
    url: &url::Url,
    receive: bool,
) -> Result<(tokio::net::TcpStream, Advertisement)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let host = url.host_str().context("missing Git host")?;
    let mut socket = tokio::net::TcpStream::connect((host, url.port().unwrap_or(9418))).await?;
    let host_header = if let Some(port) = url.port() {
        format!("{host}:{port}")
    } else {
        host.to_owned()
    };
    let service = if receive {
        "git-receive-pack"
    } else {
        "git-upload-pack"
    };
    socket
        .write_all(&crate::transport::packet(
            format!("{service} {}\0host={host_header}\0", url.path()).as_bytes(),
        )?)
        .await?;
    let mut ad = Advertisement::new();
    let mut total = 0;
    for count in 0..=100_000 {
        let mut header = [0; 4];
        socket.read_exact(&mut header).await?;
        ensure!(
            header.iter().all(u8::is_ascii_hexdigit),
            "invalid native Git packet"
        );
        let size = usize::from_str_radix(std::str::from_utf8(&header)?, 16)?;
        if size == 0 {
            return Ok((socket, ad));
        }
        ensure!(
            count < 100_000 && (4..=65520).contains(&size),
            "invalid native Git advertisement"
        );
        total += size;
        ensure!(
            total <= 8 * 1024 * 1024,
            "native Git advertisement exceeds limit"
        );
        let mut data = vec![0; size - 4];
        socket.read_exact(&mut data).await?;
        ad.line(&data, count == 0)?;
    }
    anyhow::bail!("native Git advertisement exceeds limit")
}

/// A receive-pack session bound to a caller-selected endpoint and exact ref.
pub enum Push {
    Ssh(protocol::Push),
    Native {
        socket: tokio::net::TcpStream,
        name: String,
        old: Option<Oid>,
        advertisement: Advertisement,
    },
    Local {
        repo: Repo,
        name: String,
        old: Option<Oid>,
        advertisement: Advertisement,
    },
    Http {
        client: reqwest::Client,
        base: url::Url,
        name: String,
        old: Option<Oid>,
        advertisement: Advertisement,
    },
}
fn checked_branch(repo: &Repo, mut head: Vec<u8>, name: &str) -> Result<bool> {
    let mut seen = HashSet::new();
    loop {
        let value = head.strip_suffix(b"\n").unwrap_or(&head);
        let Some(reference) = value.strip_prefix(b"ref: ") else {
            Oid::parse(value)?;
            return Ok(false);
        };
        let reference = std::str::from_utf8(reference)?;
        crate::repo::refname(reference)?;
        ensure!(
            seen.len() < 8 && seen.insert(reference.to_owned()),
            "cyclic or deep worktree HEAD"
        );
        if reference == name {
            return Ok(true);
        }
        let Some(next) = repo.read(reference, 2048)? else {
            return Ok(false);
        };
        head = next;
    }
}
fn local_push_locks(repo: &Repo, name: &str) -> Result<Vec<crate::repo::Pending>> {
    let mut locks = Vec::new();
    if repo.worktree.is_some() {
        locks.push(crate::repo::Pending::new(
            repo.root.try_clone()?,
            "HEAD.lock".into(),
        )?);
        ensure!(
            !checked_branch(
                repo,
                repo.read("HEAD", 2048)?.context("missing HEAD")?,
                name
            )?,
            "refusing to push to a checked-out branch"
        );
    }
    // Read only metadata under the opened destination. In particular, never
    // follow a linked worktree's gitdir path to discover its HEAD.
    if repo.exists("worktrees")? {
        let (dir, _) = repo.parent("worktrees/.check", false)?;
        for entry in rustix::fs::Dir::read_from(&dir)? {
            let entry = entry?;
            let child = entry.file_name().to_str()?;
            if matches!(child, "." | "..") {
                continue;
            }
            ensure!(locks.len() < 1024, "too many destination worktrees");
            let prefix = format!("worktrees/{child}");
            let (metadata, _) = repo.parent(&format!("{prefix}/.check"), false)?;
            locks.push(crate::repo::Pending::new(metadata, "HEAD.lock".into())?);
            let head = repo
                .read(&format!("{prefix}/HEAD"), 2048)?
                .context("worktree has no HEAD")?;
            ensure!(
                !checked_branch(repo, head, name)?,
                "refusing to push to a branch checked out in a linked worktree"
            );
        }
    }
    crate::linked::available(repo, name)?;
    Ok(locks)
}
pub(crate) fn http_auth(
    request: reqwest::RequestBuilder,
    base: &url::Url,
) -> Result<reqwest::RequestBuilder> {
    let token = std::env::var("SAFE_GIT_HTTP_TOKEN").ok();
    let username = std::env::var("SAFE_GIT_HTTP_USERNAME").ok();
    let password = std::env::var("SAFE_GIT_HTTP_PASSWORD").ok();
    ensure!(
        token.is_none() || (username.is_none() && password.is_none()),
        "select either HTTP Bearer or Basic authentication"
    );
    if username.is_some() || password.is_some() {
        ensure!(base.scheme() == "https", "HTTP credentials require HTTPS");
        let username =
            username.context("HTTP Basic authentication requires SAFE_GIT_HTTP_USERNAME")?;
        let password =
            password.context("HTTP Basic authentication requires SAFE_GIT_HTTP_PASSWORD")?;
        ensure!(
            !username.is_empty()
                && username.len() <= 1024
                && !username.contains(':')
                && !username.chars().any(char::is_control),
            "invalid HTTP username"
        );
        ensure!(
            !password.is_empty()
                && password.len() <= 8192
                && !password.chars().any(char::is_control),
            "invalid HTTP password"
        );
        return Ok(request.basic_auth(username, Some(password)));
    }
    if let Some(token) = token {
        ensure!(base.scheme() == "https", "HTTP tokens require HTTPS");
        ensure!(
            !token.is_empty() && token.len() <= 8192 && !token.chars().any(char::is_control),
            "invalid HTTP token"
        );
        Ok(request.bearer_auth(token))
    } else {
        Ok(request)
    }
}
impl Push {
    pub async fn prepare(url: &str, auth: &Auth, name: &str) -> Result<Self> {
        crate::repo::refname(name)?;
        match endpoint(url)? {
            Endpoint::Ssh(remote) => Ok(Self::Ssh(
                protocol::Push::prepare(&remote, auth, name).await?,
            )),
            Endpoint::Http(base) => {
                let client = client()?;
                let mut url = service_url(&base, "info/refs")?;
                url.set_query(Some("service=git-receive-pack"));
                let data = body(
                    http_auth(client.get(url), &base)?.send().await?,
                    "application/x-git-receive-pack-advertisement",
                    8 * 1024 * 1024,
                )
                .await?;
                let mut bytes = data.as_slice();
                ensure!(
                    packet(&mut bytes)? == Some(b"# service=git-receive-pack\n".as_slice()),
                    "not a smart HTTP receive service"
                );
                ensure!(packet(&mut bytes)?.is_none(), "missing service flush");
                let mut ad = Advertisement::new();
                let mut count = 0;
                while let Some(line) = packet(&mut bytes)? {
                    ensure!(count < 100000, "too many refs");
                    ad.line(line, count == 0)?;
                    count += 1;
                }
                ensure!(
                    bytes.is_empty() && ad.caps.contains("report-status"),
                    "invalid receive advertisement or missing report-status"
                );
                Ok(Self::Http {
                    client,
                    base,
                    name: name.into(),
                    old: ad.refs.get(name).copied(),
                    advertisement: ad,
                })
            }
            Endpoint::Git(base) => {
                let (socket, ad) = native_ad(&base, true).await?;
                ensure!(
                    ad.caps.contains("report-status"),
                    "server lacks report-status"
                );
                Ok(Self::Native {
                    socket,
                    name: name.into(),
                    old: ad.refs.get(name).copied(),
                    advertisement: ad,
                })
            }
            Endpoint::Local(path) => {
                let repo = Repo::open(&path)?;
                // Neither receive hooks nor receive.* configuration is executed or trusted.
                // A checked-out branch must never be changed behind its working tree.
                let _locks = local_push_locks(&repo, name)?;
                ensure!(
                    repo.shallow.is_empty(),
                    "push to a shallow local repository is unsupported"
                );
                let old = repo.read_ref(name)?;
                let mut advertisement = Advertisement::new();
                if let Some(old) = old {
                    advertisement.refs.insert(name.into(), old);
                }
                Ok(Self::Local {
                    repo,
                    name: name.into(),
                    old,
                    advertisement,
                })
            }
        }
    }
    pub fn old(&self) -> Option<Oid> {
        match self {
            Self::Ssh(p) => p.old,
            Self::Http { old, .. } => *old,
            Self::Native { old, .. } | Self::Local { old, .. } => *old,
        }
    }
    pub async fn unchanged(self) -> Result<()> {
        match self {
            Self::Ssh(p) => p.unchanged().await,
            Self::Http { .. } => Ok(()),
            Self::Local { .. } => Ok(()),
            Self::Native { mut socket, .. } => {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                socket.write_all(b"0000").await?;
                socket.shutdown().await?;
                let mut rest = Vec::new();
                socket.take(1025).read_to_end(&mut rest).await?;
                ensure!(rest.is_empty(), "unexpected response to empty push");
                Ok(())
            }
        }
    }
    pub fn advertisement(&self) -> &Advertisement {
        match self {
            Self::Ssh(p) => &p.advertisement,
            Self::Http { advertisement, .. }
            | Self::Native { advertisement, .. }
            | Self::Local { advertisement, .. } => advertisement,
        }
    }
    pub async fn send(self, new: Oid, pack: Vec<u8>) -> Result<()> {
        match self {
            Self::Ssh(p) => p.send(new, pack).await,
            Self::Local {
                repo, name, old, ..
            } => {
                let _head_locks = local_push_locks(&repo, &name)?;
                if new == Oid::ZERO {
                    if let Some(old) = old {
                        repo.delete_ref(&name, old)?;
                    }
                    return Ok(());
                }
                let objects = crate::pack::decode(&pack)?;
                let mut store = Store::new(&repo);
                store.allow_promisor = false;
                store.add(objects)?;
                if name.starts_with("refs/heads/") {
                    store.get(new)?.commit()?;
                }
                // Validate all referenced objects before publishing the destination ref.
                let ids = crate::object::reachable(&mut store, new)?;
                crate::linked::available(&repo, &name)?;
                let lock = repo.prepare_ref(&name, old)?;
                for id in ids {
                    repo.write_object(store.get(id)?.as_ref())?;
                }
                lock.commit(new)
            }
            Self::Http { ref name, old, .. } | Self::Native { ref name, old, .. } => {
                let updates = vec![protocol::RefUpdate {
                    name: name.clone(),
                    old,
                    new,
                }];
                self.send_many(&updates, false, pack).await
            }
        }
    }
    pub async fn send_many(
        self,
        updates: &[protocol::RefUpdate],
        atomic: bool,
        pack: Vec<u8>,
    ) -> Result<()> {
        match self {
            Self::Ssh(p) => p.send_many(updates, atomic, pack).await,
            Self::Local { .. } => anyhow::bail!("local push does not support atomic updates"),
            Self::Native {
                mut socket,
                advertisement,
                ..
            } => {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                socket
                    .write_all(&protocol::push_request(&advertisement, updates, atomic)?)
                    .await?;
                socket.write_all(&pack).await?;
                socket.shutdown().await?;
                let mut data = Vec::new();
                socket
                    .take(8 * 1024 * 1024 + 1)
                    .read_to_end(&mut data)
                    .await?;
                ensure!(data.len() <= 8 * 1024 * 1024, "push report exceeds limit");
                let mut bytes = data.as_slice();
                ensure!(
                    packet(&mut bytes)? == Some(b"unpack ok\n".as_slice()),
                    "remote unpack failed"
                );
                let mut remaining: HashSet<_> = updates.iter().map(|u| u.name.as_str()).collect();
                while let Some(line) = packet(&mut bytes)? {
                    protocol::push_status(line, &mut remaining)?;
                }
                ensure!(remaining.is_empty(), "missing ref status");
                Ok(())
            }
            Self::Http {
                client,
                base,
                advertisement,
                ..
            } => {
                let mut request = protocol::push_request(&advertisement, updates, atomic)?;
                request.extend(pack);
                let response =
                    http_auth(client.post(service_url(&base, "git-receive-pack")?), &base)?
                        .header(
                            reqwest::header::CONTENT_TYPE,
                            "application/x-git-receive-pack-request",
                        )
                        .header(
                            reqwest::header::ACCEPT,
                            "application/x-git-receive-pack-result",
                        )
                        .body(request)
                        .send()
                        .await?;
                let data = body(
                    response,
                    "application/x-git-receive-pack-result",
                    8 * 1024 * 1024,
                )
                .await?;
                let mut bytes = data.as_slice();
                ensure!(
                    packet(&mut bytes)? == Some(b"unpack ok\n"),
                    "remote unpack failed"
                );
                let mut remaining: HashSet<_> = updates.iter().map(|u| u.name.as_str()).collect();
                while let Some(line) = packet(&mut bytes)? {
                    protocol::push_status(line, &mut remaining)?;
                }
                ensure!(remaining.is_empty(), "missing ref status");
                Ok(())
            }
        }
    }
}
