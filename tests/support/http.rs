//! A loopback smart-HTTP Git fixture. Only the fixture invokes git http-backend.
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
pub struct Http {
    pub url: String,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Http {
    pub fn new(root: &Path) -> Self {
        Self::start(root, None)
    }
    pub fn tls(root: &Path) -> (Self, String) {
        let rcgen::CertifiedKey { cert, key_pair } =
            rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(key_pair.serialize_der()).into(),
            )
            .unwrap();
        (Self::start(root, Some(Arc::new(config))), cert.pem())
    }
    fn start(root: &Path, tls: Option<Arc<rustls::ServerConfig>>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!(
            "{}://{}",
            if tls.is_some() { "https" } else { "http" },
            listener.local_addr().unwrap()
        );
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let root = root.to_owned();
        let thread = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        if let Some(config) = &tls {
                            let conn = rustls::ServerConnection::new(config.clone()).unwrap();
                            serve(rustls::StreamOwned::new(conn, stream), &root);
                        } else {
                            serve(stream, &root);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("HTTP fixture: {e}"),
                }
            }
        });
        Self {
            url,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for Http {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
fn serve(mut stream: impl Read + Write, root: &PathBuf) {
    let mut data = Vec::new();
    let mut byte = [0];
    while !data.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).unwrap_or(0) == 0 {
            return;
        }
        data.push(byte[0]);
        assert!(data.len() < 65536);
    }
    let headers = String::from_utf8(data).unwrap();
    let mut lines = headers.split("\r\n");
    let first = lines.next().unwrap();
    let parts: Vec<_> = first.split(' ').collect();
    let (method, target) = (parts[0], parts[1]);
    let length = lines
        .clone()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .and_then(|s| s.parse::<usize>().ok())
        })
        .unwrap_or(0);
    assert!(length < 16 * 1024 * 1024);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    if target.starts_with("/rpc") {
        let body = r#"{"version":5,"type":"multiinfo","resultcount":1,"results":[{"ID":1,"Name":"fixture","PackageBaseID":1,"PackageBase":"fixture","Version":"1-1","Description":"fixture","URL":null,"NumVotes":0,"Popularity":0.0,"OutOfDate":null,"Maintainer":"test","FirstSubmitted":1,"LastModified":1,"URLPath":"/cgit/aur.git/snapshot/fixture.tar.gz"}]}"#;
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        return;
    }
    if target.starts_with("/redirect") {
        write!(stream,"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        return;
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut child = Command::new("git")
        .arg("http-backend")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REQUEST_METHOD", method)
        .env("PATH_INFO", path)
        .env("QUERY_STRING", query)
        .env("CONTENT_LENGTH", length.to_string())
        .env(
            "CONTENT_TYPE",
            lines
                .clone()
                .find_map(|l| {
                    l.split_once(':')
                        .filter(|(key, _)| key.eq_ignore_ascii_case("content-type"))
                        .map(|(_, v)| v.trim())
                })
                .unwrap_or(""),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&body).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let split = output
        .stdout
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap();
    let head = String::from_utf8(output.stdout[..split].to_vec()).unwrap();
    let body = &output.stdout[split + 4..];
    let status = head
        .lines()
        .find_map(|l| l.strip_prefix("Status: "))
        .unwrap_or("200 OK");
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    )
    .unwrap();
    for line in head.lines() {
        if !line.starts_with("Status:") {
            write!(stream, "{line}\r\n").unwrap();
        }
    }
    write!(stream, "\r\n").unwrap();
    let _ = stream.write_all(body);
}
