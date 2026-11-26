pub mod http;
pub mod password;
pub mod service;

use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, Write},
    net::{TcpListener, TcpStream},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub const BIN: &str = env!("CARGO_BIN_EXE_safe-git");

#[derive(Debug, PartialEq, Eq)]
pub struct Captured {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

pub struct TestCommand {
    command: Command,
    input: Vec<u8>,
}

impl TestCommand {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        let mut command = Command::new(program);
        // Each child gets its own environment. Never mutate the test runner's
        // environment: the suites can run concurrently.
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name.starts_with("GIT_") || name.starts_with("SAFE_GIT_") || name == "SSH_AUTH_SOCK"
            {
                command.env_remove(key);
            }
        }
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C");
        Self {
            command,
            input: Vec::new(),
        }
    }

    pub fn current_dir(mut self, path: impl AsRef<Path>) -> Self {
        self.command.current_dir(path);
        self
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.command.arg(arg);
        self
    }

    pub fn args(mut self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Self {
        self.command.args(args);
        self
    }

    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.command.env(key, value);
        self
    }

    pub fn input(mut self, input: impl AsRef<[u8]>) -> Self {
        self.input = input.as_ref().to_vec();
        self
    }

    pub fn traced(mut self, trace: &Path) -> Self {
        if available("strace") {
            let mut command = Command::new("strace");
            command
                .args(["-f", "-e", "trace=execve,execveat", "-o"])
                .arg(trace);
            command
                .arg(self.command.get_program())
                .args(self.command.get_args());
            for (key, value) in self.command.get_envs() {
                if let Some(value) = value {
                    command.env(key, value);
                } else {
                    command.env_remove(key);
                }
            }
            self.command = command;
        }
        self
    }

    pub fn output(mut self) -> Captured {
        // Files avoid pipe deadlocks while polling a bounded child lifetime,
        // even when Git writes more than a pipe buffer to either stream.
        let mut stdin = tempfile::tempfile().unwrap();
        stdin.write_all(&self.input).unwrap();
        stdin.rewind().unwrap();
        let mut stdout = tempfile::tempfile().unwrap();
        let mut stderr = tempfile::tempfile().unwrap();
        self.command
            .stdin(stdin)
            .stdout(stdout.try_clone().unwrap())
            .stderr(stderr.try_clone().unwrap());
        let description = format!("{:?}", self.command);
        let mut child = ChildGuard::new(
            self.command
                .spawn()
                .unwrap_or_else(|e| panic!("{description}: {e}")),
        );
        let deadline = Instant::now() + Duration::from_secs(40);
        let status = loop {
            if let Some(status) = child.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.child.kill().unwrap();
                child.child.wait().unwrap();
                stdout.rewind().unwrap();
                stderr.rewind().unwrap();
                let mut out = String::new();
                let mut err = String::new();
                stdout.read_to_string(&mut out).unwrap();
                stderr.read_to_string(&mut err).unwrap();
                panic!("command timed out: {description}\nstdout: {out}\nstderr: {err}");
            }
            thread::sleep(Duration::from_millis(5));
        };
        stdout.rewind().unwrap();
        stderr.rewind().unwrap();
        let mut out = String::new();
        let mut err = String::new();
        stdout.read_to_string(&mut out).unwrap();
        stderr.read_to_string(&mut err).unwrap();
        Captured {
            code: status.code(),
            stdout: out,
            stderr: err,
        }
    }

    fn checked(self, success: bool) -> String {
        let description = format!("{:?}", self.command);
        let output = self.output();
        assert_eq!(
            output.code == Some(0),
            success,
            "{description}\n{output:#?}"
        );
        format!("{}{}", output.stdout, output.stderr)
            .trim()
            .to_owned()
    }

    pub fn run(self) -> String {
        self.checked(true)
    }
    pub fn fails(self) -> String {
        self.checked(false)
    }

    pub fn spawn(mut self) -> ChildGuard {
        self.command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        ChildGuard::new(self.command.spawn().unwrap())
    }
}

pub struct ChildGuard {
    pub child: Child,
    process_group: bool,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self {
            child,
            process_group: false,
        }
    }
    pub fn signal(&self, signal: rustix::process::Signal) {
        rustix::process::kill_process(rustix::process::Pid::from_child(&self.child), signal)
            .unwrap();
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.process_group {
            // Stop the fixture's process group even when a test panics.
            let _ = rustix::process::kill_process_group(
                rustix::process::Pid::from_child(&self.child),
                rustix::process::Signal::TERM,
            );
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn git(repo: &Path) -> TestCommand {
    TestCommand::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "filter.probe.process=",
            "-c",
            "filter.probe.clean=",
            "-c",
            "filter.probe.smudge=",
            "-C",
        ])
        .arg(repo)
}

pub fn revision(repo: &Path, name: &str) -> String {
    git(repo).args(["rev-parse", name]).run()
}
pub fn head(repo: &Path) -> String {
    revision(repo, "main")
}
pub fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path).unwrap()
}
pub fn write(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) {
    fs::write(path, bytes).unwrap();
}
pub fn append(path: impl AsRef<Path>, text: &str) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}
pub fn chmod(path: impl AsRef<Path>, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}
pub fn quote(value: impl AsRef<OsStr>) -> String {
    shlex::try_quote(value.as_ref().to_str().unwrap())
        .unwrap()
        .into_owned()
}
pub fn passed(label: &str) {
    println!("PASS {label}");
}
pub fn available(program: &str) -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).any(|dir| {
        fs::metadata(dir.join(program))
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}
pub fn check_trace(path: &Path) {
    if available("strace") {
        let trace = read(path);
        let executions: Vec<_> = trace
            .lines()
            .filter(|line| line.contains("execve"))
            .collect();
        assert_eq!(executions.len(), 1, "{executions:?}");
        assert!(executions[0].contains(BIN), "{executions:?}");
    }
}

pub struct Fixture {
    // Drop the server before removing its configuration, keys and repositories.
    _server: ChildGuard,
    directory: tempfile::TempDir,
    pub port: u16,
    user: String,
}

impl Fixture {
    pub fn new() -> Self {
        Self::options(false, false, "-1m:+1h")
    }
    pub fn forwarding() -> Self {
        Self::options(true, false, "-1m:+1h")
    }
    pub fn certificate() -> Self {
        Self::options(false, true, "-1m:+1h")
    }
    pub fn expired_certificate() -> Self {
        Self::options(false, true, "-2h:-1h")
    }
    fn options(forwarding: bool, certificate: bool, validity: &str) -> Self {
        let directory = tempfile::Builder::new()
            .prefix("safe-git-test-")
            .tempdir()
            .unwrap();
        let root = directory.path();
        let user = TestCommand::new("id").arg("-un").run();
        for key in ["client_key", "host_key"] {
            TestCommand::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(root.join(key))
                .run();
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let host = read(root.join("host_key.pub"));
        let host: Vec<_> = host.split_whitespace().collect();
        write(
            root.join("known_hosts"),
            format!("[127.0.0.1]:{port} {} {}\n", host[0], host[1]),
        );
        write(
            root.join("authorized_keys"),
            format!(
                "{}{}",
                if forwarding { "" } else { "restrict " },
                read(root.join("client_key.pub"))
            ),
        );
        let config = root.join("sshd_config");
        let root_text = root.display();
        write(
            &config,
            format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {root_text}/host_key\nPidFile {root_text}/sshd.pid\nAuthorizedKeysFile {root_text}/authorized_keys\nStrictModes no\nPubkeyAuthentication yes\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nPermitRootLogin yes\nAllowUsers {user}\nDisableForwarding yes\nPermitTTY no\nPermitUserRC no\nForceCommand {} --ssh-service {}\nLogLevel ERROR\n",
                quote(std::env::current_exe().unwrap()),
                quote(root)
            ),
        );
        if forwarding {
            let text = read(&config).replace("DisableForwarding yes", "DisableForwarding no");
            write(&config, text);
        }
        if certificate {
            TestCommand::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(root.join("ca"))
                .run();
            TestCommand::new("ssh-keygen")
                .args(["-q", "-s"])
                .arg(root.join("ca"))
                .args([
                    "-I",
                    "transport-fixture",
                    "-h",
                    "-n",
                    "127.0.0.1",
                    "-V",
                    validity,
                ])
                .arg(root.join("host_key.pub"))
                .run();
            append(
                &config,
                &format!("HostCertificate {root_text}/host_key-cert.pub\n"),
            );
            write(
                root.join("known_hosts"),
                format!(
                    "@cert-authority [127.0.0.1]:{port} {}",
                    read(root.join("ca.pub"))
                ),
            );
        }
        if Command::new("/usr/bin/sshd")
            .args(["-T", "-f"])
            .arg(&config)
            .args(["-o", "PerSourcePenalties=no"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
        {
            append(&config, "PerSourcePenalties no\n");
        }
        let log_path = root.join("sshd.log");
        let log = File::create(&log_path).unwrap();
        drop(listener);
        // The server uses the current user, temporary keys and an unprivileged
        // loopback port, so it does not need root or sudo.
        let child = Command::new("/usr/bin/sshd")
            .args(["-D", "-e", "-f"])
            .arg(&config)
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .process_group(0)
            .spawn()
            .expect("start test sshd as the current user");
        let mut server = ChildGuard {
            child,
            process_group: true,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "test sshd exited: {}",
                read(&log_path)
            );
            if TcpStream::connect_timeout(
                &format!("127.0.0.1:{port}").parse().unwrap(),
                Duration::from_millis(100),
            )
            .is_ok()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "test sshd did not start: {}",
                read(&log_path)
            );
            thread::sleep(Duration::from_millis(50));
        }
        Self {
            _server: server,
            directory,
            port,
            user,
        }
    }

    pub fn root(&self) -> &Path {
        self.directory.path()
    }
    pub fn url(&self, repo: &Path) -> String {
        format!(
            "ssh://{}@127.0.0.1:{}{}",
            self.user,
            self.port,
            repo.display()
        )
    }
    pub fn safe(&self, repo: &Path) -> TestCommand {
        TestCommand::new(BIN)
            .env("SAFE_GIT_STATE_DIR", self.root().join("state"))
            .arg("-C")
            .arg(repo)
            .arg("--identity-file")
            .arg(self.root().join("client_key"))
            .arg("--known-hosts")
            .arg(self.root().join("known_hosts"))
            .args(["--timeout", "20"])
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if thread::panicking() {
            eprintln!("sshd log:\n{}", read(self.root().join("sshd.log")));
        }
    }
}
