mod archive;
mod checkout;
mod clone;
mod compat;
mod config;
mod history;
mod ignore;
mod index;
mod network;
mod object;
mod output;
mod pack;
mod packed;
mod patch;
mod porcelain;
mod protocol;
mod push;
mod repo;
mod revision;
mod sequence;
mod stash;
mod transport;
mod trust;
mod workflow;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use object::{Oid, ReadObjects, ancestor, commits, reachable, reachable_since};
use repo::{Repo, Store};
use std::{path::PathBuf, time::Duration};
use transport::Auth;

#[derive(Parser)]
#[command(
    version = env!("SAFE_GIT_VERSION"),
    about = "Git operations without repository-directed execution",
    after_help = "Everyday commands: status, add, switch, restore, diff, log, commit, stash.\nAdditional commands: ls-remote, config, rev-parse, symbolic-ref, update-ref,\nmerge-base, branch, reset, reflog, checkout, rebase, cherry-pick, revert, tag --list, rev-list, describe,\narchive --format tar. Unsupported options fail closed."
)]
struct Cli {
    /// Repository or a directory within its working tree
    #[arg(short = 'C', default_value = ".", global = true)]
    repository: PathBuf,
    #[arg(skip)]
    prefix: PathBuf,
    /// Use only this private key instead of the host's agent and default keys
    #[arg(
        long,
        global = true,
        conflicts_with = "ssh_agent",
        env = "SAFE_GIT_IDENTITY_FILE"
    )]
    identity_file: Option<PathBuf>,
    /// Use only SSH_AUTH_SOCK instead of also trying default keys; never forward it
    #[arg(long, global = true, env = "SAFE_GIT_SSH_AGENT")]
    ssh_agent: bool,
    /// Caller-selected endpoint for fetch/pull/push without a positional URL
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
    /// Inert configuration values for this invocation; never enables commands
    #[arg(short = 'c', global = true)]
    config: Vec<String>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Clone verified objects into a new or empty repository
    Clone {
        /// Select a branch instead of the remote's advertised default
        #[arg(short, long)]
        branch: Option<String>,
        /// Fetch only the selected branch
        #[arg(long)]
        single_branch: bool,
        #[arg(long)]
        bare: bool,
        #[arg(long)]
        mirror: bool,
        #[arg(short = 'o', long, default_value = "origin")]
        origin: String,
        #[arg(long)]
        no_progress: bool,
        /// Accepted for compatibility; objects are copied and verified, never shared
        #[arg(short = 's', long)]
        shared: bool,
        url: String,
        path: Option<PathBuf>,
    },
    /// Create a new SHA-1 repository
    Init {
        #[arg(long)]
        bare: bool,
        #[arg(short = 'b', long, default_value = "main")]
        initial_branch: String,
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Fetch one branch from an explicit SSH URL into a tracking ref
    Fetch {
        url: Option<String>,
        branch: Option<String>,
        /// Storage label only; never resolves a configured remote
        #[arg(long, default_value = "origin")]
        remote: String,
        #[arg(long)]
        all: bool,
        #[arg(short, long)]
        prune: bool,
        #[arg(short, long)]
        verbose: bool,
        #[arg(long)]
        tags: bool,
    },
    /// Fetch one branch and integrate it without repository-selected commands
    Pull {
        #[arg(long)]
        ff_only: bool,
        /// Submodules are always inert; accepted for Git command compatibility
        #[arg(long)]
        no_recurse_submodules: bool,
        #[arg(long)]
        no_stat: bool,
        url: Option<String>,
        /// Defaults to the current branch; repository upstream config is ignored
        branch: Option<String>,
    },
    /// Merge a revision; conflicts can be continued or aborted
    Merge {
        #[arg(long)]
        ff_only: bool,
        #[arg(long)]
        no_stat: bool,
        #[arg(long)]
        no_edit: bool,
        #[arg(long)]
        no_ff: bool,
        #[arg(long)]
        no_commit: bool,
        #[arg(long)]
        r#continue: bool,
        #[arg(long)]
        abort: bool,
        reference: Option<String>,
    },
    /// Push branches or tags to a caller-selected or host-registered endpoint
    Push {
        /// Remember the destination in trusted host state, never repository config
        #[arg(short = 'u', long)]
        set_upstream: bool,
        url: Option<String>,
        /// BRANCH or LOCAL:REMOTE; defaults to current HEAD branch
        refspec: Vec<String>,
        #[arg(short, long)]
        force: bool,
        #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "")]
        force_with_lease: Option<String>,
        #[arg(long)]
        delete: bool,
        #[arg(long)]
        tags: bool,
    },
    /// Recover an interrupted safe-git working-tree update from its verified backups
    Recover,
    /// Additional inert Git commands used by package managers
    #[command(external_subcommand)]
    Compatibility(Vec<String>),
}

impl Cli {
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
    if let Command::Compatibility(args) = &cli.command {
        return compat::run(cli, args).await;
    }
    if let Command::Clone {
        url,
        path,
        branch,
        single_branch,
        bare,
        mirror,
        origin,
        ..
    } = &cli.command
    {
        let path = path.clone().unwrap_or_else(|| {
            let name = url
                .trim_end_matches('/')
                .rsplit(['/', ':'])
                .next()
                .unwrap_or("repository");
            PathBuf::from(if *bare || *mirror {
                name.to_owned()
            } else {
                name.trim_end_matches(".git").to_owned()
            })
        });
        return clone::run(
            &cli.repository.join(&path),
            &path,
            &network::normalize_from(url, &cli.repository)?,
            &cli.auth()?,
            clone::Options {
                branch: branch.as_deref(),
                remote: origin,
                single: *single_branch,
                bare: *bare || *mirror,
                mirror: *mirror,
                quiet: cli.quiet,
            },
        )
        .await;
    }
    if let Command::Init {
        path,
        initial_branch,
        bare,
    } = &cli.command
    {
        let path = cli.repository.join(path);
        if *bare {
            Repo::init(&path, initial_branch)?;
        } else {
            if !path.exists() {
                std::fs::create_dir(&path)?;
            }
            Repo::init_worktree(&path, initial_branch)?;
        }
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
        Command::Clone { .. }
        | Command::Init { .. }
        | Command::Recover
        | Command::Compatibility(_) => unreachable!(),
        Command::Fetch {
            url,
            branch,
            remote,
            prune,
            all,
            ..
        } => {
            if branch.is_none()
                && cli.remote_url.is_none()
                && (url.is_none()
                    || trust::get(&repo)?.is_some_and(|b| url.as_deref() == Some(&b.remote)))
            {
                compat::fetch_all(cli, &repo, &mut store, *prune).await?;
            } else {
                ensure!(
                    !all && !prune,
                    "--all/--prune require the registered remote"
                );
                fetch(
                    cli,
                    &repo,
                    &mut store,
                    url.as_deref(),
                    branch.as_deref(),
                    Some(remote),
                )
                .await?;
            }
        }
        Command::Pull {
            url,
            branch,
            no_stat,
            ff_only,
            ..
        } => {
            let head = repo.head()?;
            let binding = trust::get(&repo)?;
            let endpoint = trust::endpoint(&repo, url.as_deref(), cli.remote_url.as_deref())?;
            let default_branch = binding
                .as_ref()
                .filter(|b| b.url == endpoint)
                .and_then(|b| {
                    head.strip_prefix("refs/heads/")
                        .and_then(|h| b.upstreams.get(h))
                });
            let incoming = fetch(
                cli,
                &repo,
                &mut store,
                url.as_deref(),
                branch.as_deref().or(default_branch.map(String::as_str)),
                None,
            )
            .await?;
            ensure!(repo.head()? == head, "HEAD changed during pull; retry");
            if *ff_only {
                merge(&repo, &mut store, incoming, cli.quiet, *no_stat)?;
            } else {
                sequence::merge(cli, &repo, &mut store, incoming, false, false)?;
            }
        }
        Command::Merge {
            reference,
            no_stat,
            ff_only,
            no_ff,
            no_commit,
            r#continue,
            abort,
            ..
        } => {
            if *r#continue || *abort {
                ensure!(!(*r#continue && *abort), "incompatible merge controls");
                return sequence::run(
                    cli,
                    &repo,
                    &mut store,
                    "merge",
                    &[if *abort {
                        "--abort".into()
                    } else {
                        "--continue".into()
                    }],
                );
            }
            let reference = match reference {
                Some(r) => r.clone(),
                None => revision::upstream(&repo)?,
            };
            let incoming = revision::commit(&repo, &mut store, &reference)?;
            if *ff_only {
                ensure!(!no_ff && !no_commit, "incompatible merge options");
                merge(&repo, &mut store, incoming, cli.quiet, *no_stat)?;
            } else {
                sequence::merge(cli, &repo, &mut store, incoming, *no_commit, *no_ff)?;
            }
        }
        Command::Push {
            url,
            refspec,
            set_upstream,
            force,
            force_with_lease,
            delete,
            tags,
        } => {
            push::run(
                cli,
                &repo,
                &mut store,
                push::Options {
                    url: url.as_deref(),
                    specs: refspec,
                    upstream: *set_upstream,
                    force: *force,
                    lease: force_with_lease.as_deref(),
                    delete: *delete,
                    tags: *tags,
                },
            )
            .await?;
        }
    }
    Ok(())
}

async fn fetch(
    cli: &Cli,
    repo: &Repo,
    store: &mut Store<'_>,
    url: Option<&str>,
    branch: Option<&str>,
    remote: Option<&str>,
) -> Result<Oid> {
    let head = repo.head()?;
    let branch =
        repo::branch(branch.unwrap_or(head.strip_prefix("refs/heads/").context("invalid HEAD")?))?;
    let reference = remote
        .map(|remote| repo::tracking(remote, &branch))
        .transpose()?;
    let old = match &reference {
        Some(reference) => repo.read_ref(reference)?,
        None => None,
    };
    let mut haves = Vec::new();
    for id in [old, repo.read_ref(&repo.head()?)?].into_iter().flatten() {
        if !haves.contains(&id) {
            store.get(id)?.commit()?;
            haves.push(id);
        }
    }
    let boundary = commits(store, &haves)?;
    let endpoint = trust::endpoint(repo, url, cli.remote_url.as_deref())?;
    let endpoint = network::normalize_from(&endpoint, &cli.repository)?;
    let ad = network::discover(&endpoint, &cli.auth()?).await?;
    let tip = ad.branch(&branch).context("remote branch does not exist")?;
    let objects = network::fetch(
        &endpoint,
        &cli.auth()?,
        &std::collections::HashMap::from([(format!("refs/heads/{branch}"), tip)]),
        &haves,
    )
    .await?;
    store.add(objects)?;
    store.get(tip)?.commit()?;
    let ids = reachable_since(store, tip, &boundary)?;
    for id in ids {
        repo.write_object(store.get(id)?.as_ref())?;
    }
    let fast_forward = match old {
        Some(old) if old != tip => ancestor(store, old, tip)?,
        _ => true,
    };
    if let Some(reference) = reference {
        repo.update_ref(&reference, old, tip)?;
    } else {
        repo.atomic(
            "FETCH_HEAD",
            format!("{tip}\t\tbranch '{branch}' of {endpoint}\n").as_bytes(),
        )?;
    }
    if !cli.quiet {
        if let Some(remote) = remote {
            eprint!(
                "{}",
                output::fetch(&endpoint, &branch, remote, old, tip, fast_forward)
            );
        } else {
            eprint!("{}", output::pull_fetch(&endpoint, &branch));
        }
    }
    Ok(tip)
}

fn merge(
    repo: &Repo,
    store: &mut Store<'_>,
    incoming: Oid,
    quiet: bool,
    no_stat: bool,
) -> Result<()> {
    let head = repo.head()?;
    let current = repo.read_ref(&head)?;
    store.get(incoming)?.commit()?;
    if let Some(current) = current {
        store.get(current)?.commit()?;
        if current == incoming {
            if !quiet {
                println!("Already up to date.");
            }
            return Ok(());
        }
        if !ancestor(store, current, incoming)? {
            if ancestor(store, incoming, current)? {
                if !quiet {
                    println!("Already up to date.");
                }
                return Ok(());
            }
            anyhow::bail!("Not possible to fast-forward, aborting.");
        }
    }
    // Re-check HEAD as well as using the branch's normal compare-and-swap lock.
    ensure!(repo.head()? == head, "HEAD changed concurrently; retry");
    let stat = if !quiet && !no_stat && current.is_some() {
        Some(output::diffstat(store, current, incoming))
    } else {
        None
    };
    if repo.worktree.is_some() {
        checkout::merge(repo, store, &head, current, incoming)?;
    } else {
        reachable(store, incoming)?;
        repo.update_ref(&head, current, incoming)?;
    }
    if !quiet {
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
    Ok(())
}

#[tokio::main]
async fn main() {
    let mut cli = Cli::parse();
    let location = locate(&mut cli);
    let result = if let Err(e) = location {
        Err(e)
    } else {
        tokio::time::timeout(Duration::from_secs(cli.timeout), run(&cli))
            .await
            .context("operation timed out")
            .and_then(|r| r)
    };
    if let Err(error) = result {
        if let Some(status) = error.downcast_ref::<compat::Exit>() {
            std::process::exit(status.0);
        }
        // Escape all diagnostics so server/repository strings cannot control the terminal.
        eprintln!("fatal: {}", format!("{error:#}").escape_debug());
        std::process::exit(128);
    }
}

fn locate(cli: &mut Cli) -> Result<()> {
    if matches!(cli.command, Command::Init { .. } | Command::Clone { .. })
        || matches!(&cli.command,Command::Compatibility(args) if args.first().is_some_and(|a| a == "ls-remote"))
    {
        return Ok(());
    }
    let start = std::env::current_dir()?.join(&cli.repository);
    let mut candidate = start.as_path();
    loop {
        if std::fs::symlink_metadata(candidate.join(".git")).is_ok()
            || (candidate.join("HEAD").is_file() && candidate.join("objects").is_dir())
        {
            cli.prefix = start.strip_prefix(candidate)?.to_owned();
            cli.repository = candidate.to_owned();
            return Ok(());
        }
        candidate = candidate.parent().context("no repository found")?;
    }
}
