//! Password-only SSH fixture; no system users or credentials are modified.
use super::*;
use russh::{
    Channel, ChannelId,
    server::{self, Msg, Server as _, Session},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone)]
struct Handler {
    attempts: Arc<AtomicUsize>,
}
impl server::Server for Handler {
    type Handler = Self;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        self.clone()
    }
}
impl server::Handler for Handler {
    type Error = anyhow::Error;
    async fn auth_password(&mut self, user: &str, password: &str) -> anyhow::Result<server::Auth> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Ok(if user == "test" && password == "caller-only-password" {
            server::Auth::Accept
        } else {
            server::Auth::reject()
        })
    }
    async fn channel_open_session(
        &mut self,
        _: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut Session,
    ) -> anyhow::Result<()> {
        reply.accept().await;
        Ok(())
    }
    async fn exec_request(
        &mut self,
        channel: ChannelId,
        command: &[u8],
        session: &mut Session,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            command == b"git-upload-pack '/repo'",
            "unexpected fixture command"
        );
        session.channel_success(channel)?;
        let mut data = Vec::new();
        for line in [
            "1111111111111111111111111111111111111111 HEAD\0symref=HEAD:refs/heads/main\n",
            "1111111111111111111111111111111111111111 refs/heads/main\n",
        ] {
            data.extend_from_slice(format!("{:04x}{line}", line.len() + 4).as_bytes());
        }
        data.extend_from_slice(b"0000");
        session.data(channel, data)?;
        Ok(())
    }
    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> anyhow::Result<()> {
        session.exit_status_request(channel, 0)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
}
pub struct Password {
    pub url: String,
    pub known: std::path::PathBuf,
    pub attempts: Arc<AtomicUsize>,
    handle: server::RunningServerHandle,
    thread: Option<thread::JoinHandle<()>>,
}
impl Password {
    pub fn new(root: &Path) -> Self {
        let keypath = root.join("password_host");
        TestCommand::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&keypath)
            .run();
        let key = russh::keys::load_secret_key(&keypath, None).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let known = root.join("password_known");
        write(
            &known,
            format!("[127.0.0.1]:{port} {}", read(keypath.with_extension("pub"))),
        );
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut handler = Handler {
            attempts: attempts.clone(),
        };
        let (send, recv) = std::sync::mpsc::channel();
        let thread = thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(async move {
                    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                    let config = server::Config {
                        keys: vec![key],
                        auth_rejection_time: Duration::from_millis(1),
                        ..Default::default()
                    };
                    let server = handler.run_on_socket(Arc::new(config), &listener);
                    send.send(server.handle()).unwrap();
                    server.await.unwrap();
                });
        });
        Self {
            url: format!("ssh://test@127.0.0.1:{port}/repo"),
            known,
            attempts,
            handle: recv.recv().unwrap(),
            thread: Some(thread),
        }
    }
}
impl Drop for Password {
    fn drop(&mut self) {
        self.handle.shutdown("fixture completed".into());
        self.thread.take().unwrap().join().unwrap();
    }
}
