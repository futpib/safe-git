mod checkout;
mod index;
mod object;
mod output;
mod pack;
mod packed;
mod protocol;
mod repo;
mod transport;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use object::{ReadObjects, ancestor, commits, reachable, reachable_since};
use repo::{Repo, Store};
use std::{path::PathBuf, time::Duration};
use transport::{Auth, Remote};

#[derive(Parser)]
#[command(
    version,
    about = "Restricted fetch, fast-forward merge, and push over SSH"
)]
struct Cli {
    /// Working-tree root or bare repository; never discovers parent repositories
    #[arg(short = 'C', default_value = ".", global = true)]
    repository: PathBuf,
    /// Explicit unencrypted private key (use an agent for encrypted keys)
    #[arg(
        long,
        global = true,
        conflicts_with = "ssh_agent",
        env = "SAFE_GIT_IDENTITY_FILE"
    )]
    identity_file: Option<PathBuf>,
    /// Use SSH_AUTH_SOCK for authentication; never forward the agent
    #[arg(long, global = true, env = "SAFE_GIT_SSH_AGENT")]
    ssh_agent: bool,
    /// Caller-selected endpoint for fetch/push without a positional URL
    #[arg(long = "url", global = true, env = "SAFE_GIT_URL")]
    remote_url: Option<String>,
    /// Trusted host-key file (defaults to ~/.ssh/known_hosts)
    #[arg(long, global = true)]
    known_hosts: Option<PathBuf>,
    /// Network operation deadline in seconds
    #[arg(long, default_value_t = 120, global = true, value_parser = clap::value_parser!(u64).range(1..=3600))]
    timeout: u64,
    /// Suppress successful operation summaries
    #[arg(short, long, global = true)]
    quiet: bool,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Create a new bare SHA-1 repository
    Init {
        #[arg(long, required = true)]
        bare: bool,
        #[arg(long, default_value = "main")]
        initial_branch: String,
        path: PathBuf,
    },
    /// Fetch one branch from an explicit SSH URL into a tracking ref
    Fetch {
        url: Option<String>,
        branch: Option<String>,
        /// Storage label only; never resolves a configured remote
        #[arg(long, default_value = "origin")]
        remote: String,
    },
    /// Fast-forward the current branch and update its working tree, if present
    Merge {
        #[arg(long, required = true)]
        ff_only: bool,
        /// Omit the diffstat
        #[arg(long)]
        no_stat: bool,
        /// origin/main or refs/remotes/origin/main
        reference: String,
    },
    /// Push one local branch to one remote branch, fast-forward only
    Push {
        url: Option<String>,
        /// BRANCH or LOCAL:REMOTE; defaults to current HEAD branch
        refspec: Option<String>,
    },
    /// Recover an interrupted safe-git working-tree update from its verified backups
    Recover,
}

impl Cli {
    fn url<'a>(&'a self, positional: Option<&'a str>) -> Result<&'a str> {
        positional.or(self.remote_url.as_deref()).context("provide a URL, --url, or SAFE_GIT_URL; repository remote configuration is not consulted")
    }
    fn endpoint(&self, positional: Option<&str>) -> Result<Remote> {
        Remote::parse(self.url(positional)?)
    }
    fn auth(&self) -> Result<Auth> {
        let known_hosts = self
            .known_hosts
            .clone()
            .or_else(|| std::env::home_dir().map(|h| h.join(".ssh/known_hosts")))
            .context("no home directory; provide --known-hosts")?;
        Ok(Auth {
            identity: self.identity_file.clone(),
            agent: self.ssh_agent,
            known_hosts,
        })
    }
}

async fn run(cli: &Cli) -> Result<()> {
    if let Command::Init {
        path,
        initial_branch,
        ..
    } = &cli.command
    {
        Repo::init(path, initial_branch)?;
        if !cli.quiet {
            println!(
                "Initialized empty Git repository in {}/",
                path.canonicalize()?.display().to_string().escape_debug()
            );
        }
        return Ok(());
    }
    let repo = Repo::open(&cli.repository)?;
    if matches!(cli.command, Command::Recover) {
        checkout::recover(&repo)?;
        if !cli.quiet {
            println!("Recovered interrupted checkout");
        }
        return Ok(());
    }
    ensure!(
        !repo.exists("safe-git-checkout")?,
        "interrupted checkout; run safe-git recover"
    );
    let mut store = Store::new(&repo);
    match &cli.command {
        Command::Init { .. } | Command::Recover => unreachable!(),
        Command::Fetch {
            url,
            branch,
            remote,
        } => {
            let head = repo.head()?;
            let branch = repo::branch(
                branch
                    .as_deref()
                    .unwrap_or(head.strip_prefix("refs/heads/").context("invalid HEAD")?),
            )?;
            let reference = repo::tracking(remote, &branch)?;
            let old = repo.read_ref(&reference)?;
            let mut haves = Vec::new();
            for id in [old, repo.read_ref(&repo.head()?)?].into_iter().flatten() {
                if !haves.contains(&id) {
                    store.get(id)?.commit()?;
                    haves.push(id);
                }
            }
            let boundary = commits(&mut store, &haves)?;
            let (tip, objects) = protocol::fetch(
                &cli.endpoint(url.as_deref())?,
                &cli.auth()?,
                &branch,
                None,
                &haves,
            )
            .await?;
            store.add(objects)?;
            store.get(tip)?.commit()?;
            let ids = reachable_since(&mut store, tip, &boundary)?;
            for id in ids {
                repo.write_object(store.get(id)?.as_ref())?;
            }
            let fast_forward = match old {
                Some(old) if old != tip => ancestor(&mut store, old, tip)?,
                _ => true,
            };
            repo.update_ref(&reference, old, tip)?;
            if !cli.quiet {
                eprint!(
                    "{}",
                    output::fetch(
                        cli.url(url.as_deref())?,
                        &branch,
                        remote,
                        old,
                        tip,
                        fast_forward
                    )
                );
            }
        }
        Command::Merge {
            reference, no_stat, ..
        } => {
            let reference = if reference.starts_with("refs/") {
                reference.clone()
            } else {
                format!("refs/remotes/{reference}")
            };
            ensure!(
                reference.starts_with("refs/remotes/"),
                "merge requires a fetched tracking ref"
            );
            repo::refname(&reference)?;
            let incoming = repo
                .read_ref(&reference)?
                .context("tracking ref does not exist; fetch first")?;
            let head = repo.head()?;
            let current = repo.read_ref(&head)?;
            store.get(incoming)?.commit()?;
            if let Some(current) = current {
                store.get(current)?.commit()?;
                if current == incoming {
                    if !cli.quiet {
                        println!("Already up to date.");
                    }
                    return Ok(());
                }
                if !ancestor(&mut store, current, incoming)? {
                    if ancestor(&mut store, incoming, current)? {
                        if !cli.quiet {
                            println!("Already up to date.");
                        }
                        return Ok(());
                    }
                    anyhow::bail!("Not possible to fast-forward, aborting.");
                }
            }
            // Re-check HEAD as well as using the branch's normal compare-and-swap lock.
            ensure!(repo.head()? == head, "HEAD changed concurrently; retry");
            let stat = if !cli.quiet && !no_stat && current.is_some() {
                Some(output::diffstat(&mut store, current, incoming))
            } else {
                None
            };
            if repo.worktree.is_some() {
                checkout::merge(&repo, &mut store, &head, current, incoming)?;
            } else {
                reachable(&mut store, incoming)?;
                repo.update_ref(&head, current, incoming)?;
            }
            if !cli.quiet {
                if let Some(current) = current {
                    println!(
                        "Updating {}..{}\nFast-forward",
                        output::short(current),
                        output::short(incoming)
                    );
                }
                if let Some(stat) = stat {
                    match stat {
                        Ok(stat) => print!("{stat}"),
                        Err(e) => eprintln!(
                            "warning: safe-git: diffstat omitted ({})",
                            format!("{e:#}").escape_debug()
                        ),
                    }
                }
            }
        }
        Command::Push { url, refspec } => {
            let head = repo.head()?;
            let default = head.strip_prefix("refs/heads/").context("invalid HEAD")?;
            let spec = refspec.as_deref().unwrap_or(default);
            let (local, dest) = spec.split_once(':').unwrap_or((spec, spec));
            let local = repo::branch(local)?;
            let dest = repo::branch(dest)?;
            let tip = repo
                .read_ref(&format!("refs/heads/{local}"))?
                .context("local branch does not exist")?;
            store.get(tip)?.commit()?;
            let remote = cli.endpoint(url.as_deref())?;
            let auth = cli.auth()?;
            let push = protocol::Push::prepare(&remote, &auth, &dest).await?;
            if push.old == Some(tip) {
                push.unchanged().await?;
                if !cli.quiet {
                    eprintln!("Everything up-to-date");
                }
                return Ok(());
            }
            let destination = push.old;
            // No live SSH channel while potentially expensive local object work
            // runs. Re-advertise and compare the destination again before sending.
            push.unchanged().await?;
            if let Some(old) = destination {
                match store.get(old) {
                    Ok(_) => {}
                    Err(e) if e.downcast_ref::<repo::MissingObject>().is_some() => {
                        let (_, objects) =
                            protocol::fetch(&remote, &auth, &dest, Some(old), &[tip]).await?;
                        store.add(objects)?;
                    }
                    Err(e) => return Err(e),
                }
                store.get(old)?.commit()?;
                ensure!(
                    ancestor(&mut store, old, tip)?,
                    "non-fast-forward push refused; destination is unchanged"
                );
            }
            let boundary = commits(&mut store, &destination.into_iter().collect::<Vec<_>>())?;
            let ids = reachable_since(&mut store, tip, &boundary)?;
            let objects = ids
                .into_iter()
                .map(|id| store.get(id))
                .collect::<Result<Vec<_>>>()?;
            let pack = pack::encode(&objects)?;
            let push = protocol::Push::prepare(&remote, &auth, &dest).await?;
            ensure!(
                push.old == destination,
                "push rejected: destination changed while preparing transfer; retry"
            );
            push.send(tip, pack).await?;
            if !cli.quiet {
                eprint!(
                    "{}",
                    output::push(cli.url(url.as_deref())?, &local, &dest, destination, tip)
                );
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result = tokio::time::timeout(Duration::from_secs(cli.timeout), run(&cli))
        .await
        .context("operation timed out")
        .and_then(|r| r);
    if let Err(error) = result {
        // Escape all diagnostics so server/repository strings cannot control the terminal.
        eprintln!("fatal: {}", format!("{error:#}").escape_debug());
        std::process::exit(128);
    }
}
