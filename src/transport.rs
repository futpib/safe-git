use crate::object::MAX_PACK;
use anyhow::{Context, Result, bail, ensure};
use russh::{
    Channel, ChannelMsg, client,
    keys::{
        self, PrivateKeyWithHashAlg, PublicKeyOrCertificate,
        agent::{AgentIdentity, client::AgentClient},
    },
};
use std::{collections::VecDeque, path::PathBuf, sync::Arc, time::Duration};

#[derive(Clone, Debug)]
pub struct Remote {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub path: String,
    pub(crate) explicit_port: bool,
}
impl Remote {
    pub fn parse(input: &str) -> Result<Self> {
        ensure!(
            input.len() <= 4096 && !input.chars().any(char::is_control),
            "invalid SSH URL"
        );
        let remote = if input.starts_with("ssh://") {
            let url = url::Url::parse(input)?;
            ensure!(
                url.password().is_none() && url.query().is_none() && url.fragment().is_none(),
                "SSH URL cannot contain password, query, or fragment"
            );
            ensure!(
                !input.contains('%') && !input.chars().any(char::is_whitespace),
                "escaped SSH URLs are unsupported; use scp syntax for literal paths"
            );
            Self {
                host: url
                    .host_str()
                    .context("SSH URL has no host")?
                    .trim_matches(['[', ']'])
                    .to_owned(),
                port: url.port().unwrap_or(22),
                user: url.username().to_owned(),
                path: url.path().to_owned(),
                explicit_port: url.port().is_some(),
            }
        } else {
            ensure!(!input.contains("://"), "only SSH transport is supported");
            ensure!(!input.contains("::"), "remote helper syntax is unsupported");
            let (user, rest) = input.split_once('@').unwrap_or(("", input));
            let (host, path) = rest.split_once(':').context("SSH URL has no path")?;
            Self {
                user: user.to_owned(),
                host: host.to_owned(),
                port: 22,
                path: path.to_owned(),
                explicit_port: false,
            }
        };
        ensure!(
            remote
                .user
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
            "invalid SSH username"
        );
        ensure!(
            !remote.host.is_empty()
                && remote
                    .host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-:".contains(&b)),
            "invalid SSH host"
        );
        ensure!(
            remote.port != 0 && !remote.path.is_empty() && !remote.path.starts_with('-'),
            "invalid SSH port or repository path"
        );
        Ok(remote)
    }
    fn command(&self, receive: bool) -> String {
        let service = if receive {
            "git-receive-pack"
        } else {
            "git-upload-pack"
        };
        // SSH's exec request is interpreted by the remote login shell. Quote the
        // single path argument; never run a local process or accept a command.
        format!("{service} '{}'", self.path.replace('\'', "'\\''"))
    }
}

pub struct Auth {
    pub identity: Option<PathBuf>,
    pub agent: bool,
    pub known_hosts: PathBuf,
    pub known_hosts_explicit: bool,
}

async fn authenticate_agent(
    session: &mut client::Handle<HostKey>,
    user: &str,
    hash_alg: Option<keys::HashAlg>,
) -> Result<bool> {
    let mut agent = AgentClient::connect_env()
        .await
        .context("connect SSH_AUTH_SOCK")?;
    let identities = agent.request_identities().await?;
    ensure!(identities.len() <= 64, "too many SSH agent identities");
    for identity in identities {
        if let AgentIdentity::PublicKey { key, .. } = identity
            && session
                .authenticate_publickey_with(user, key, hash_alg, &mut agent)
                .await?
                .success()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn authenticate_defaults(
    session: &mut client::Handle<HostKey>,
    user: &str,
    hash_alg: Option<keys::HashAlg>,
) -> Result<bool> {
    // The host environment and home directory are trusted. Repository config
    // never participates in selecting credentials or executing authentication.
    if std::env::var_os("SSH_AUTH_SOCK").is_some()
        && authenticate_agent(session, user, hash_alg)
            .await
            .unwrap_or(false)
    {
        return Ok(true);
    }
    if let Some(home) = std::env::home_dir() {
        for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
            // Missing, unreadable, and encrypted defaults are skipped. Encrypted
            // keys can be loaded into the host's agent; there are no prompts.
            let Ok(key) = keys::load_secret_key(home.join(".ssh").join(name), None) else {
                continue;
            };
            if session
                .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg))
                .await?
                .success()
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

struct HostKey {
    host: String,
    port: u16,
    known_hosts: PathBuf,
}
impl client::Handler for HostKey {
    type Error = anyhow::Error;
    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool> {
        crate::host_keys::verify(&self.known_hosts, &self.host, self.port, key)?;
        Ok(true)
    }
}

trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Stream for T {}

pub struct Wire {
    _jumps: Vec<client::Handle<HostKey>>,
    _session: client::Handle<HostKey>,
    channel: Channel<client::Msg>,
    buffer: VecDeque<u8>,
    received: usize,
    diagnostics: Vec<u8>,
    exit: Option<u32>,
    closed: bool,
}
impl Wire {
    pub async fn connect(remote: &Remote, auth: &Auth, receive: bool) -> Result<Self> {
        ensure!(
            !(auth.identity.is_some() && auth.agent),
            "--identity-file and --ssh-agent cannot be used together"
        );
        let mut route = crate::ssh_config::route(remote, auth)?
            .into_iter()
            .peekable();
        let first = route.peek().context("missing SSH destination")?;
        let mut stream: Box<dyn Stream> = Box::new(
            tokio::net::TcpStream::connect((first.remote.host.as_str(), first.remote.port))
                .await
                .context("connect SSH")?,
        );
        let mut jumps = Vec::new();
        while let Some(settings) = route.next() {
            let remote = &settings.remote;
            let certificates =
                crate::host_keys::has_authority(&settings.known_hosts, &remote.host, remote.port)?;
            let handler = HostKey {
                host: remote.host.clone(),
                port: remote.port,
                known_hosts: settings.known_hosts,
            };
            let mut config = client::Config {
                inactivity_timeout: Some(Duration::from_secs(30)),
                ..Default::default()
            };
            if certificates {
                config.preferred.host_key_certificates = config.preferred.key.clone();
            }
            let mut session = client::connect_stream(Arc::new(config), stream, handler)
                .await
                .context("connect SSH")?;
            let password_var = if route.peek().is_some() {
                "SAFE_GIT_SSH_JUMP_PASSWORD"
            } else {
                "SAFE_GIT_SSH_PASSWORD"
            };
            let password = std::env::var(password_var).ok();
            let hash_alg = session.best_supported_rsa_hash().await?.flatten();
            let authenticated = if let Some(password) = password {
                ensure!(
                    !password.is_empty()
                        && password.len() <= 8192
                        && !password.chars().any(char::is_control),
                    "invalid SSH password"
                );
                session
                    .authenticate_password(&remote.user, password)
                    .await?
                    .success()
            } else if let Some(path) = &settings.identity {
                let key = keys::load_secret_key(path, None)
                    .context("load SSH key (use an agent for encrypted keys)")?;
                session
                    .authenticate_publickey(
                        &remote.user,
                        PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg),
                    )
                    .await?
                    .success()
            } else if settings.agent {
                authenticate_agent(&mut session, &remote.user, hash_alg).await?
            } else {
                authenticate_defaults(&mut session, &remote.user, hash_alg).await?
            };
            ensure!(
                authenticated,
                "SSH authentication failed; provide a host-selected key, agent or password"
            );
            if let Some(next) = route.peek() {
                let channel = session
                    .channel_open_direct_tcpip(
                        &next.remote.host,
                        u32::from(next.remote.port),
                        "127.0.0.1",
                        0,
                    )
                    .await?;
                stream = Box::new(channel.into_stream());
                jumps.push(session);
            } else {
                let channel = session.channel_open_session().await?;
                channel.exec(true, remote.command(receive)).await?;
                return Ok(Self {
                    _jumps: jumps,
                    _session: session,
                    channel,
                    buffer: VecDeque::new(),
                    received: 0,
                    diagnostics: Vec::new(),
                    exit: None,
                    closed: false,
                });
            }
        }
        bail!("missing SSH destination")
    }
    pub async fn send(&self, data: Vec<u8>) -> Result<()> {
        self.channel.data_bytes(data).await?;
        Ok(())
    }
    pub async fn eof(&self) -> Result<()> {
        self.channel.eof().await?;
        Ok(())
    }
    async fn receive(&mut self) -> Result<()> {
        match self.channel.wait().await {
            Some(ChannelMsg::Data { data }) => {
                self.received += data.len();
                ensure!(
                    self.received <= MAX_PACK + 8 * 1024 * 1024,
                    "SSH response exceeds limit"
                );
                self.buffer.extend(data.iter().copied());
            }
            Some(ChannelMsg::ExtendedData { data, .. }) => {
                ensure!(
                    self.diagnostics.len() + data.len() <= 64 * 1024,
                    "SSH diagnostics exceed limit"
                );
                self.diagnostics.extend_from_slice(&data);
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => self.exit = Some(exit_status),
            Some(ChannelMsg::ExitSignal { .. } | ChannelMsg::Failure) => {
                bail!("remote Git service failed")
            }
            Some(ChannelMsg::Close) | None => self.closed = true,
            Some(ChannelMsg::Eof | ChannelMsg::Success | ChannelMsg::WindowAdjusted { .. }) => {}
            _ => bail!("unexpected SSH channel message"),
        }
        Ok(())
    }
    fn success(&self) -> Result<()> {
        ensure!(
            self.exit == Some(0),
            "remote Git service failed (status {:?}): {:?}",
            self.exit,
            String::from_utf8_lossy(&self.diagnostics)
        );
        Ok(())
    }
    async fn exact(&mut self, count: usize) -> Result<Vec<u8>> {
        while self.buffer.len() < count && !self.closed {
            self.receive().await?;
        }
        if self.buffer.len() < count {
            self.success()?;
            bail!("truncated Git protocol response");
        }
        Ok(self.buffer.drain(..count).collect())
    }
    pub async fn packet(&mut self) -> Result<Option<Vec<u8>>> {
        let header = self.exact(4).await?;
        let size = packet_size(&header)?;
        if size == 0 {
            return Ok(None);
        }
        Ok(Some(self.exact(size - 4).await?))
    }
    pub async fn remainder(&mut self) -> Result<Vec<u8>> {
        let mut out: Vec<_> = self.buffer.drain(..).collect();
        while !self.closed {
            self.receive().await?;
            ensure!(
                out.len() + self.buffer.len() <= MAX_PACK,
                "response body exceeds limit"
            );
            out.extend(self.buffer.drain(..));
        }
        self.success()?;
        Ok(out)
    }
}

pub fn packet(data: &[u8]) -> Result<Vec<u8>> {
    ensure!(data.len() <= 65516, "packet exceeds protocol limit");
    let mut out = format!("{:04x}", data.len() + 4).into_bytes();
    out.extend_from_slice(data);
    Ok(out)
}
fn packet_size(header: &[u8]) -> Result<usize> {
    ensure!(
        header.len() == 4 && header.iter().all(u8::is_ascii_hexdigit),
        "invalid packet header"
    );
    let size = usize::from_str_radix(std::str::from_utf8(header)?, 16)?;
    ensure!(
        size == 0 || (4..=65520).contains(&size),
        "unsupported packet size/control packet"
    );
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn urls_and_quoting() {
        let url = Remote::parse("git@github.com:futpib/folio.git").unwrap();
        assert_eq!(url.host, "github.com");
        assert_eq!(url.path, "futpib/folio.git");
        assert_eq!(url.command(false), "git-upload-pack 'futpib/folio.git'");
        let url = Remote::parse("ssh://claude@localhost:2222/tmp/repo").unwrap();
        assert_eq!(url.port, 2222);
        let url = Remote::parse("claude@localhost:/tmp/a'b;$(touch x)").unwrap();
        assert_eq!(
            url.command(true),
            "git-receive-pack '/tmp/a'\\''b;$(touch x)'"
        );
        for bad in [
            "ext::command",
            "https://host/x",
            "git@host:-x",
            "ssh://git:secret@host/x",
            "git@host:a\nb",
            "ssh://git@host/x?y",
            "ssh://git@host/%2f",
        ] {
            assert!(Remote::parse(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn framing() {
        assert_eq!(packet(b"NAK\n").unwrap(), b"0008NAK\n");
        assert_eq!(packet_size(b"0000").unwrap(), 0);
        for bad in [b"0001", b"0002", b"ffff", b"xx00"] {
            assert!(packet_size(bad).is_err());
        }
    }
}
