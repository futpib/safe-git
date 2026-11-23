//! Real Git/OpenSSH interoperability tests. This executable also serves as the
//! temporary sshd's forced command, keeping the entire fixture in Rust.
#[path = "suites/bootstrap.rs"]
mod bootstrap;
#[path = "suites/completion.rs"]
mod completion;
#[path = "suites/extended.rs"]
mod extended;
#[path = "suites/gaps.rs"]
mod gaps;
#[path = "suites/http_packages.rs"]
mod http_packages;
#[path = "suites/output.rs"]
mod output;
#[path = "suites/packages.rs"]
mod packages;
#[path = "support/mod.rs"]
mod support;
#[path = "suites/transfers.rs"]
mod transfers;
#[path = "suites/workflow.rs"]
mod workflow;
#[path = "suites/worktree.rs"]
mod worktree;

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|arg| arg == "--ssh-service") {
        let root = args.get(2).expect("SSH fixture root");
        if let Err(error) = support::service::run(std::path::Path::new(root)) {
            eprintln!("SSH test service: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    let args = libtest_mimic::Arguments::from_args();
    libtest_mimic::run(
        &args,
        vec![
            libtest_mimic::Trial::test("completion_inspection", || {
                completion::inspection();
                Ok(())
            }),
            libtest_mimic::Trial::test("completion_patches", || {
                completion::patches();
                Ok(())
            }),
            libtest_mimic::Trial::test("completion_rebase", || {
                completion::rebase();
                Ok(())
            }),
            libtest_mimic::Trial::test("completion_push", || {
                completion::push();
                Ok(())
            }),
            libtest_mimic::Trial::test("gaps_edge_safety", || {
                gaps::edge_safety();
                Ok(())
            }),
            libtest_mimic::Trial::test("gaps_inspection_files", || {
                gaps::inspection_files();
                Ok(())
            }),
            libtest_mimic::Trial::test("gaps_tags_remotes", || {
                gaps::tags_remotes();
                Ok(())
            }),
            libtest_mimic::Trial::test("gaps_sequences_bisect", || {
                gaps::sequences_bisect();
                Ok(())
            }),
            libtest_mimic::Trial::test("gaps_linked_submodules", || {
                gaps::linked_submodules();
                Ok(())
            }),
            libtest_mimic::Trial::test("gaps_shallow_network", || {
                gaps::shallow_network();
                Ok(())
            }),
            libtest_mimic::Trial::test("extended_ssh_push", || {
                extended::ssh_push();
                Ok(())
            }),
            libtest_mimic::Trial::test("extended_history", || {
                extended::history();
                Ok(())
            }),
            libtest_mimic::Trial::test("extended_stash_status", || {
                extended::stash_status();
                Ok(())
            }),
            libtest_mimic::Trial::test("extended_sequences", || {
                extended::sequences();
                Ok(())
            }),
            libtest_mimic::Trial::test("extended_http_push", || {
                extended::http_push();
                Ok(())
            }),
            libtest_mimic::Trial::test("workflow_ignores", || {
                workflow::ignores();
                Ok(())
            }),
            libtest_mimic::Trial::test("workflow", || {
                workflow::suite();
                Ok(())
            }),
            libtest_mimic::Trial::test("workflow_network", || {
                workflow::network();
                Ok(())
            }),
            libtest_mimic::Trial::test("http_packages", || {
                http_packages::suite();
                Ok(())
            }),
            libtest_mimic::Trial::test("packages", || {
                packages::suite();
                Ok(())
            }),
            libtest_mimic::Trial::test("bootstrap", || {
                bootstrap::suite();
                Ok(())
            }),
            libtest_mimic::Trial::test("transfers", || {
                transfers::suite();
                Ok(())
            }),
            libtest_mimic::Trial::test("worktree", || {
                worktree::suite();
                Ok(())
            }),
            libtest_mimic::Trial::test("output", || {
                output::suite();
                Ok(())
            }),
        ],
    )
    .exit();
}
