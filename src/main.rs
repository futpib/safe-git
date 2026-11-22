mod archive;
mod checkout;
mod clone;
mod compat;
mod config;
mod ignore;
mod index;
mod network;
mod object;
mod output;
mod pack;
mod packed;
mod porcelain;
mod protocol;
mod repo;
mod revision;
mod stash;
mod transport;
mod trust;
mod workflow;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use object::{Oid, ReadObjects, ancestor, commits, reachable, reachable_since};
use repo::{Repo, Store};
use std::{path::PathBuf, time::Duration};
use transport::{Auth, Remote};

#[derive(Parser)]
#[command(
    version = env!("SAFE_GIT_VERSION"),
    about = "Git operations without repository-directed execution",
    after_help = "Everyday commands: status, add, switch, restore, diff, log, commit, stash.\nAdditional commands: ls-remote, config, rev-parse, symbolic-ref, update-ref,\nmerge-base, reset --hard, checkout, rebase, tag --list, rev-list, describe,\narchive --format tar. Unsupported options fail closed."
)]
struct Cli {
    /// Working-tree root or bare repository; never discovers parent repositories
    #[arg(short = 'C', default_value = ".", global = true)]
    repository: PathBuf,
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
        #[arg(long)]
        all: bool,
        #[arg(short, long)]
        prune: bool,
        #[arg(short, long)]
        verbose: bool,
        #[arg(long)]
        tags: bool,
    },
    /// Fetch one branch and fast-forward under one repository lock
    Pull {
        #[arg(long, required = true)]
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
    /// Fast-forward the current branch and update its working tree, if present
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
        reference: Option<String>,
    },
    /// Push one local branch to one remote branch, fast-forward only
    Push {
        /// Remember the destination in trusted host state, never repository config
        #[arg(short = 'u', long)]
        set_upstream: bool,
        url: Option<String>,
        /// BRANCH or LOCAL:REMOTE; defaults to current HEAD branch
        refspec: Option<String>,
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
            merge(&repo, &mut store, incoming, cli.quiet, *no_stat)?;
        }
        Command::Merge {
            reference,
            no_stat,
            ff_only,
            no_ff,
            no_commit,
            ..
        } => {
            let reference = match reference {
                Some(r) => r.clone(),
                None => revision::upstream(&repo)?,
            };
            let incoming = revision::commit(&repo, &mut store, &reference)?;
            if *ff_only {
                ensure!(!no_ff && !no_commit, "incompatible merge options");
                merge(&repo, &mut store, incoming, cli.quiet, *no_stat)?;
            } else {
                ensure!(
                    *no_ff && *no_commit,
                    "merge requires --ff-only or --no-ff --no-commit"
                );
                porcelain::review_merge(&repo, &mut store, incoming, cli.quiet)?;
            }
        }
        Command::Push {
            url,
            refspec,
            set_upstream,
        } => {
            let head = repo.head()?;
            let default = head.strip_prefix("refs/heads/").context("invalid HEAD")?;
            let endpoint = trust::endpoint(&repo, url.as_deref(), cli.remote_url.as_deref())?;
            let endpoint = network::normalize_from(&endpoint, &cli.repository)?;
            let mut binding = trust::get(&repo)?;
            let remembered = binding
                .as_ref()
                .filter(|b| b.url == endpoint)
                .and_then(|b| b.upstreams.get(default))
                .map(|dest| format!("{default}:{dest}"));
            let spec = refspec
                .as_deref()
                .or(remembered.as_deref())
                .unwrap_or(default);
            let (local, dest) = spec.split_once(':').unwrap_or((spec, spec));
            let local = repo::branch(local)?;
            let dest = repo::branch(dest)?;
            let tip = repo
                .read_ref(&format!("refs/heads/{local}"))?
                .context("local branch does not exist")?;
            store.get(tip)?.commit()?;
            let remote = Remote::parse(&endpoint)?;
            if *set_upstream {
                let b = binding.get_or_insert_with(|| trust::Binding {
                    url: endpoint.clone(),
                    remote: url
                        .as_deref()
                        .filter(|v| !v.contains([':', '/']))
                        .unwrap_or("origin")
                        .into(),
                    branch: local.clone(),
                    single: false,
                    mirror: false,
                    bare: repo.worktree.is_none(),
                    upstreams: Default::default(),
                });
                if b.url != endpoint {
                    b.upstreams.clear();
                }
                b.url = endpoint.clone();
                b.upstreams.insert(local.clone(), dest.clone());
                ensure!(
                    serde_json::to_vec(b)?.len() <= 16384,
                    "host endpoint record exceeds limit"
                );
            }
            let auth = cli.auth()?;
            let push = protocol::Push::prepare(&remote, &auth, &dest).await?;
            if push.old == Some(tip) {
                push.unchanged().await?;
                if *set_upstream {
                    trust::save(&repo, binding.as_ref().unwrap())?;
                }
                pushed_tracking_ref(&repo, binding.as_ref(), &endpoint, &dest, tip)?;
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
            if *set_upstream {
                trust::save(&repo, binding.as_ref().unwrap())?;
            }
            pushed_tracking_ref(&repo, binding.as_ref(), &endpoint, &dest, tip)?;
            if !cli.quiet {
                eprint!(
                    "{}",
                    output::push(&endpoint, &local, &dest, destination, tip)
                );
            }
        }
    }
    Ok(())
}

fn pushed_tracking_ref(
    repo: &Repo,
    binding: Option<&trust::Binding>,
    endpoint: &str,
    branch: &str,
    tip: Oid,
) -> Result<()> {
    if let Some(binding) = binding.filter(|b| b.url == endpoint && !b.bare) {
        let reference = repo::tracking(&binding.remote, branch)?;
        let old = repo.read_ref(&reference)?;
        repo.update_ref(&reference, old, tip)
            .context("push succeeded, but updating the local tracking ref failed")?;
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
    let cli = Cli::parse();
    let result = tokio::time::timeout(Duration::from_secs(cli.timeout), run(&cli))
        .await
        .context("operation timed out")
        .and_then(|r| r);
    if let Err(error) = result {
        if let Some(status) = error.downcast_ref::<compat::Exit>() {
            std::process::exit(status.0);
        }
        // Escape all diagnostics so server/repository strings cannot control the terminal.
        eprintln!("fatal: {}", format!("{error:#}").escape_debug());
        std::process::exit(128);
    }
}
