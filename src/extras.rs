//! Additional data-only workflows. No helpers, aliases, editors or merge drivers.
use crate::{
    Cli,
    repo::{Repo, Store},
};
use anyhow::Result;
pub fn run(
    cli: &Cli,
    repo: &Repo,
    store: &mut Store<'_>,
    name: &str,
    args: &[String],
) -> Result<()> {
    match name {
        "bundle" => crate::bundle::run(cli, repo, store, args),
        "notes" => crate::notes::run(cli, repo, store, args),
        "range-diff" => crate::range_diff::run(repo, store, args),
        "rerere" => crate::rerere::run(cli, repo, store, args),
        _ => unreachable!(),
    }
}

pub fn write_file(cli: &Cli, name: &str, bytes: &[u8]) -> Result<()> {
    use anyhow::{Context, ensure};
    use std::io::Write;
    if name == "-" {
        std::io::stdout().lock().write_all(bytes)?;
        return Ok(());
    }
    let path = std::env::current_dir()?
        .join(&cli.repository)
        .join(&cli.prefix)
        .join(name);
    let mut normalized = std::path::PathBuf::from("/");
    for part in path.components() {
        match part {
            std::path::Component::Normal(p) => normalized.push(p),
            std::path::Component::ParentDir => {
                ensure!(normalized.pop(), "path escapes root");
            }
            _ => {}
        }
    }
    let parent =
        crate::linked::open_directory(normalized.parent().context("missing output parent")?)?;
    let name = normalized
        .file_name()
        .context("missing output name")?
        .to_str()
        .context("non-UTF-8 output name")?;
    ensure!(
        matches!(
            rustix::fs::statat(&parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW),
            Err(rustix::io::Errno::NOENT)
        ),
        "output already exists"
    );
    let mut lock = crate::repo::Pending::new(parent, format!("{name}.lock"))?;
    lock.file.write_all(bytes)?;
    // Publish without replacing a file created while the output was prepared.
    lock.file.sync_all()?;
    lock.commit_noreplace(name)
}
