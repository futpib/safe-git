//! Restricted server-side fixture for real Git, malformed packs and ref races.
use anyhow::{Context, Result, ensure};
use sha1_checked::{Digest, Sha1};
use std::{
    fs,
    io::{self, Read, Write},
    os::unix::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
};

fn packet(output: &mut impl Write, data: &[u8]) -> Result<()> {
    write!(output, "{:04x}", data.len() + 4)?;
    output.write_all(data)?;
    Ok(())
}

fn read_packet(input: &mut impl Read) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut header = vec![0; 4];
    input.read_exact(&mut header)?;
    let size = usize::from_str_radix(std::str::from_utf8(&header)?, 16)?;
    ensure!(
        size == 0 || (4..=65520).contains(&size),
        "invalid packet length"
    );
    let mut body = vec![0; size.saturating_sub(4)];
    input.read_exact(&mut body)?;
    Ok((header, body))
}

pub fn run(root: &Path) -> Result<()> {
    let original = std::env::var("SSH_ORIGINAL_COMMAND")?;
    let args = shlex::split(&original).context("invalid command quoting")?;
    ensure!(
        args.len() == 2
            && matches!(args[0].as_str(), "git-upload-pack" | "git-receive-pack")
            && [root.join("source.git"), root.join("dest.git")]
                .contains(&Path::new(&args[1]).to_path_buf()),
        "unexpected command: {original}"
    );
    let mut stdout = io::stdout().lock();
    if root.join("malformed").exists() {
        let mode = fs::read_to_string(root.join("malformed"))?;
        if mode == "frame" {
            stdout.write_all(b"ffff")?;
            return Ok(());
        }
        packet(
            &mut stdout,
            b"ffffffffffffffffffffffffffffffffffffffff refs/heads/main\0ofs-delta\n",
        )?;
        stdout.write_all(b"0000")?;
        stdout.flush()?;
        let mut stdin = io::stdin().lock();
        loop {
            if read_packet(&mut stdin)?.1 == b"done\n" {
                break;
            }
        }
        let mut pack = b"PACK\x00\x00\x00\x02\x00\x00\x00\x00".to_vec();
        let checksum = if mode == "missing" {
            Sha1::digest(&pack).to_vec()
        } else {
            vec![0; 20]
        };
        pack.extend_from_slice(&checksum);
        packet(&mut stdout, b"NAK\n")?;
        stdout.write_all(&pack)?;
        return Ok(());
    }
    let mut command = Command::new("/usr/bin/git");
    command.args([&args[0][4..], &args[1]]);
    let trailer = root.join("push-trailer");
    if args[0] == "git-receive-pack" && trailer.exists() {
        let child = command.stdout(Stdio::piped()).spawn()?;
        let mut child = super::ChildGuard::new(child);
        let mut output = child.child.stdout.take().context("receive-pack stdout")?;
        let mut report = Vec::new();
        let mut buffer = [0; 65536];
        loop {
            let count = output.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            report.extend_from_slice(&buffer[..count]);
            stdout.write_all(&buffer[..count])?;
            stdout.flush()?;
        }
        let status = child.child.wait()?;
        // safe-git first probes receive-pack with an empty push. That exchange
        // has no status report, so it must not receive a report trailer.
        if report
            .windows(b"unpack ".len())
            .any(|window| window == b"unpack ")
        {
            stdout.write_all(&fs::read(trailer)?)?;
        }
        stdout.flush()?;
        ensure!(status.success(), "receive-pack failed");
        return Ok(());
    }
    let race = root.join("race");
    if args[0] == "git-receive-pack"
        && Path::new(&args[1]) == root.join("dest.git")
        && race.exists()
    {
        let counter = root.join("race-count");
        let seen: usize = if counter.exists() {
            fs::read_to_string(&counter)?.parse()?
        } else {
            0
        };
        fs::write(counter, (seen + 1).to_string())?;
        if seen != 0 {
            let child = command.stdout(Stdio::piped()).spawn()?;
            let mut child = super::ChildGuard::new(child);
            let mut output = child.child.stdout.take().context("receive-pack stdout")?;
            // Buffer the whole advertisement until the destination has moved.
            let mut advertisement = Vec::new();
            loop {
                let (header, body) = read_packet(&mut output)?;
                advertisement.extend_from_slice(&header);
                advertisement.extend_from_slice(&body);
                if header == b"0000" {
                    break;
                }
            }
            let target = fs::read_to_string(&race)?;
            fs::remove_file(race)?;
            let status = Command::new("/usr/bin/git")
                .args(["-C", &args[1], "update-ref", "refs/heads/main", &target])
                .status()?;
            ensure!(status.success(), "could not move destination ref");
            stdout.write_all(&advertisement)?;
            stdout.flush()?;
            io::copy(&mut output, &mut stdout)?;
            stdout.flush()?;
            ensure!(child.child.wait()?.success(), "receive-pack failed");
            return Ok(());
        }
    }
    Err(command.exec().into())
}
