mod archive;
mod binary_patch;
mod bisect;
mod checkout;
mod clone;
mod compat;
mod config;
mod files;
mod history;
mod host_keys;
mod ignore;
mod index;
mod inspect;
mod linked;
mod mail;
mod network;
mod object;
mod output;
mod pack;
mod packed;
mod partial;
mod patch;
mod porcelain;
mod protocol;
mod push;
mod rebase_plan;
mod remotes;
mod repo;
mod revision;
mod search;
mod sequence;
mod shallow;
mod sparse;
mod ssh_config;
mod stash;
mod static_http;
mod submodule;
mod tags;
mod transport;
mod trust;
mod workflow;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use object::{Oid, ReadObjects, ancestor, reachable};
use repo::{Repo, Store};
use std::{path::PathBuf, time::Duration};
use transport::Auth;

#[derive(Parser)]
#[command(
    version = env!("SAFE_GIT_VERSION"),
    about = "Git operations without repository-directed execution",
    after_help = "Everyday commands: status, add, switch, restore, diff, log, commit, stash.\nAdditional commands: ls-remote, config, rev-parse, symbolic-ref, update-ref,\nmerge-base, branch, reset, reflog, checkout, rebase, cherry-pick, revert, tag, rev-list, describe,\narchive --format tar, show, blame, rm, mv, clean, remote, worktree, bisect, submodule, grep, apply, am, format-patch. Unsupported options fail closed."
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
        /// Omit historical blobs; missing content uses the host-registered endpoint
        #[arg(long)]
        filter: Option<String>,
        #[arg(short = 'n', long)]
        no_checkout: bool,
        #[arg(long,value_parser=clap::value_parser!(u32).range(1..))]
        depth: Option<u32>,
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
        #[arg(long, value_parser=clap::value_parser!(u32).range(1..), conflicts_with_all=["unshallow", "depth"])]
        deepen: Option<u32>,
        #[arg(long, value_parser=clap::value_parser!(u32).range(1..), conflicts_with="unshallow")]
        depth: Option<u32>,
        #[arg(long)]
        unshallow: bool,
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
        #[arg(long, conflicts_with = "ff_only")]
        rebase: bool,
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
        /// Push every local branch
        #[arg(long, conflicts_with_all = ["tags", "delete"])]
        all: bool,
        /// Require the server to update every requested ref or none
        #[arg(long)]
        atomic: bool,
        /// Validate the push without updating refs or upstreams
        #[arg(short = 'n', long)]
        dry_run: bool,
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
            known_hosts_explicit: self.known_hosts.is_some(),
        })
    }
}

async fn run(cli: &Cli) -> Result<()> {
    if let Command::Compatibility(args) = &cli.command {
        return compat::run(cli, args).await;
    }
    if let Command::Clone {
        filter,
        no_checkout,
        depth,
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
                filter: filter.as_deref(),
                no_checkout: *no_checkout,
                depth: *depth,
                branch: branch.as_deref(),
                remote: origin,
                single: *single_branch || depth.is_some(),
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
    ensure!(
        !repo.exists("safe-git-am")?
            || !matches!(cli.command, Command::Pull { .. } | Command::Merge { .. }),
        "am in progress; continue or abort it first"
    );
    let mut store = Store::new(&repo);
    match &cli.command {
        Command::Clone { .. }
        | Command::Init { .. }
        | Command::Recover
        | Command::Compatibility(_) => unreachable!(),
        Command::Fetch {
            unshallow,
            url,
            branch,
            remote,
            prune,
            all,
            ..
        } => {
            let registered = trust::selected(&repo, url.as_deref())?;
            if *unshallow {
                ensure!(!store.shallow.is_empty(), "repository is not shallow");
                let endpoint = network::normalize_from(
                    &trust::endpoint(&repo, url.as_deref(), cli.remote_url.as_deref())?,
                    &cli.repository,
                )?;
                let ad = network::discover(&endpoint, &cli.auth()?).await?;
                let refs = ad
                    .refs
                    .into_iter()
                    .filter(|(r, _)| r.starts_with("refs/heads/"))
                    .collect();
                let objects = network::fetch(&endpoint, &cli.auth()?, &refs, &[]).await?;
                store.add(objects)?;
                let boundaries = std::mem::take(&mut store.shallow);
                for id in boundaries {
                    for object in reachable(&mut store, id)? {
                        repo.write_object(store.get(object)?.as_ref())?;
                    }
                }
                repo.remove("shallow")?;
            }
            if *all {
                ensure!(
                    url.is_none() && branch.is_none() && cli.remote_url.is_none(),
                    "--all takes no endpoint or branch"
                );
                let registry = trust::registry(&repo)?;
                ensure!(!registry.remotes.is_empty(), "no registered remotes");
                for binding in registry.remotes.values() {
                    compat::fetch_binding(cli, &repo, &mut store, *prune, binding).await?;
                }
            } else if branch.is_none() && cli.remote_url.is_none() && registered.is_some() {
                compat::fetch_binding(cli, &repo, &mut store, *prune, registered.as_ref().unwrap())
                    .await?;
            } else {
                ensure!(
                    !prune,
                    "--prune requires a registered remote without a branch"
                );
                fetch(
                    cli,
                    &repo,
                    &mut store,
                    url.as_deref(),
                    branch.as_deref(),
                    Some(
                        registered
                            .as_ref()
                            .map(|b| b.remote.as_str())
                            .unwrap_or(remote),
                    ),
                )
                .await?;
            }
        }
        Command::Pull {
            rebase,
            url,
            branch,
            no_stat,
            ff_only,
            ..
        } => {
            let head = repo.head()?;
            let binding = trust::selected(&repo, url.as_deref())?;
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
            if *rebase {
                sequence::run(cli, &repo, &mut store, "rebase", &[incoming.to_string()])?;
            } else if *ff_only {
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
            all,
            atomic,
            dry_run,
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
                    all: *all,
                    atomic: *atomic,
                    dry_run: *dry_run,
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

fn fetch_options(cli: &Cli, store: &Store<'_>, filter: Option<String>) -> network::FetchOptions {
    let (depth, deepen) = match &cli.command {
        Command::Fetch { depth, deepen, .. } => (*depth, *deepen),
        _ => (None, None),
    };
    network::FetchOptions {
        depth,
        deepen,
        shallow: store.shallow.clone(),
        filter,
        raw: false,
    }
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
    if !store.shallow.is_empty() {
        haves.clear();
    }
    let endpoint = trust::endpoint(repo, url, cli.remote_url.as_deref())?;
    let endpoint = network::normalize_from(&endpoint, &cli.repository)?;
    let ad = network::discover(&endpoint, &cli.auth()?).await?;
    let tip = ad.branch(&branch).context("remote branch does not exist")?;
    let binding = trust::selected(repo, url)?;
    let filter = binding.filter(|b| b.url == endpoint).and_then(|b| b.filter);
    let options = fetch_options(cli, store, filter.clone());
    let boundary = partial::boundary(store, &haves, &options)?;
    let fetched = network::fetch_options(
        &endpoint,
        &cli.auth()?,
        &std::collections::HashMap::from([(format!("refs/heads/{branch}"), tip)]),
        &haves,
        &options,
    )
    .await?;
    if filter.is_some() {
        partial::store_pack(repo, &fetched.objects)?;
    }
    store.shallow = fetched.shallow;
    store.add(fetched.objects)?;
    store.get(tip)?.commit()?;
    let seen = partial::persist(repo, store, &[tip], filter.is_some(), &boundary)?;
    shallow::validate(store, &seen, &options.shallow)?;
    shallow::save(repo, &store.shallow)?;
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
    partial::configure(&cli);
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
        || matches!(&cli.command,Command::Compatibility(args) if args.first().is_some_and(|a| a == "ls-remote" || (a == "config" && args.iter().any(|a| a == "--global"))))
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
