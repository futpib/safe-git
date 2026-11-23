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
fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(120))
        .user_agent(concat!("safe-git/", env!("CARGO_PKG_VERSION")))
        .build()?)
}
fn service_url(base: &url::Url, suffix: &str) -> Result<url::Url> {
    Ok(url::Url::parse(&format!(
        "{}/{suffix}",
        base.as_str().trim_end_matches('/')
    ))?)
}
async fn body(mut response: reqwest::Response, kind: &str, limit: usize) -> Result<Vec<u8>> {
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
async fn http_ad(client: &reqwest::Client, base: &url::Url) -> Result<Advertisement> {
    let mut url = service_url(base, "info/refs")?;
    url.set_query(Some("service=git-upload-pack"));
    let data = body(
        http_auth(client.get(url), base)?.send().await?,
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
    Ok(ad)
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
        Endpoint::Http(base) => http_ad(&client()?, &base).await,
        Endpoint::Git(base) => {
            let (mut socket, ad) = native_ad(&base).await?;
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
    let haves = if depth.is_some() { &[][..] } else { haves };
    if refs.is_empty() {
        return Ok(Fetched {
            objects: Objects::new(),
            shallow: HashSet::new(),
        });
    }
    match endpoint(url)? {
        Endpoint::Ssh(remote) => {
            if let Some(depth) = depth {
                protocol::fetch_depth(&remote, auth, refs, depth).await
            } else {
                Ok(Fetched {
                    objects: protocol::fetch_many(&remote, auth, refs, haves).await?,
                    shallow: HashSet::new(),
                })
            }
        }
        Endpoint::Http(base) => {
            let client = client()?;
            let ad = http_ad(&client, &base).await?;
            for (name, id) in refs {
                ensure!(
                    ad.refs.get(name) == Some(id),
                    "remote ref changed while fetching; retry"
                );
            }
            let tips: HashSet<_> = refs.values().copied().collect();
            let request =
                protocol::request_depth(&ad, &tips.into_iter().collect::<Vec<_>>(), haves, depth)?;
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
            let mut shallow = HashSet::new();
            if depth.is_some() {
                while let Some(line) = packet(&mut bytes)? {
                    protocol::shallow_line(line, &mut shallow)?;
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
            let (mut socket, ad) = native_ad(&base).await?;
            for (name, id) in refs {
                ensure!(
                    ad.refs.get(name) == Some(id),
                    "remote ref changed while fetching; retry"
                );
            }
            let tips: HashSet<_> = refs.values().copied().collect();
            socket
                .write_all(&protocol::request_depth(
                    &ad,
                    &tips.into_iter().collect::<Vec<_>>(),
                    haves,
                    depth,
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
            let mut shallow = HashSet::new();
            if depth.is_some() {
                while let Some(line) = packet(&mut bytes)? {
                    protocol::shallow_line(line, &mut shallow)?;
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
            let mut objects = Objects::new();
            let mut total = 0;
            for (name, id) in refs {
                ensure!(
                    ad.refs.get(name) == Some(id),
                    "local source ref changed while fetching; retry"
                );
            }
            let mut shallow = store.shallow.clone();
            if let Some(depth) = depth {
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
                objects.insert(id, obj);
            }
            Ok(Fetched { objects, shallow })
        }
    }
}

async fn native_ad(url: &url::Url) -> Result<(tokio::net::TcpStream, Advertisement)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let host = url.host_str().context("missing Git host")?;
    let mut socket = tokio::net::TcpStream::connect((host, url.port().unwrap_or(9418))).await?;
    let host_header = if let Some(port) = url.port() {
        format!("{host}:{port}")
    } else {
        host.to_owned()
    };
    socket
        .write_all(&crate::transport::packet(
            format!("git-upload-pack {}\0host={host_header}\0", url.path()).as_bytes(),
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
    Http {
        client: reqwest::Client,
        base: url::Url,
        name: String,
        old: Option<Oid>,
        delete_refs: bool,
    },
}
fn http_auth(request: reqwest::RequestBuilder, base: &url::Url) -> Result<reqwest::RequestBuilder> {
    if let Ok(token) = std::env::var("SAFE_GIT_HTTP_TOKEN") {
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
                    delete_refs: ad.caps.contains("delete-refs"),
                })
            }
            _ => anyhow::bail!("push requires SSH or HTTP(S)"),
        }
    }
    pub fn old(&self) -> Option<Oid> {
        match self {
            Self::Ssh(p) => p.old,
            Self::Http { old, .. } => *old,
        }
    }
    pub async fn unchanged(self) -> Result<()> {
        match self {
            Self::Ssh(p) => p.unchanged().await,
            Self::Http { .. } => Ok(()),
        }
    }
    pub async fn send(self, new: Oid, pack: Vec<u8>) -> Result<()> {
        match self {
            Self::Ssh(p) => p.send(new, pack).await,
            Self::Http {
                client,
                base,
                name,
                old,
                delete_refs,
            } => {
                ensure!(
                    new != Oid::ZERO || delete_refs,
                    "server does not allow deletion"
                );
                let mut request = crate::transport::packet(
                    format!("{} {new} {name}\0report-status\n", old.unwrap_or(Oid::ZERO))
                        .as_bytes(),
                )?;
                request.extend_from_slice(b"0000");
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
                    packet(&mut bytes)? == Some(b"unpack ok\n".as_slice()),
                    "remote unpack failed"
                );
                let expected = format!("ok {name}\n");
                ensure!(
                    packet(&mut bytes)? == Some(expected.as_bytes()),
                    "remote rejected push"
                );
                ensure!(packet(&mut bytes)?.is_none(), "unexpected extra ref status");
                Ok(())
            }
        }
    }
}
