//! Real Git/OpenSSH interoperability tests. This executable also serves as the
//! temporary sshd's forced command, keeping the entire fixture in Rust.
#[path = "suites/bootstrap.rs"]
mod bootstrap;
#[path = "suites/closure.rs"]
mod closure;
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
#[path = "suites/remaining.rs"]
mod remaining;
#[path = "support/mod.rs"]
mod support;
#[path = "suites/transfers.rs"]
mod transfers;
#[path = "suites/transport_gaps.rs"]
mod transport_gaps;
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
            libtest_mimic::Trial::test("closure_sha256_ssh_native", || {
                closure::sha256_ssh_native();
                Ok(())
            }),
            libtest_mimic::Trial::test("closure_sha256_workflows", || {
                closure::sha256_workflows();
                Ok(())
            }),
            libtest_mimic::Trial::test("closure_sha256_storage_transport", || {
                closure::sha256_storage_transport();
                Ok(())
            }),
            libtest_mimic::Trial::test("closure_review", || {
                closure::review();
                Ok(())
            }),
            libtest_mimic::Trial::test("closure_interactive", || {
                closure::interactive();
                Ok(())
            }),
            libtest_mimic::Trial::test("closure_rebase", || {
                closure::rebase();
                Ok(())
            }),
            libtest_mimic::Trial::test("closure_maintenance_signing", || {
                closure::maintenance_signing();
                Ok(())
            }),
            libtest_mimic::Trial::test("closure_indexes", || {
                closure::indexes();
                Ok(())
            }),
            libtest_mimic::Trial::test("closure_mail_patches", || {
                closure::mail_patches();
                Ok(())
            }),
            libtest_mimic::Trial::test("closure_submodules", || {
                closure::submodules();
                Ok(())
            }),
            libtest_mimic::Trial::test("remaining_sparse", || {
                remaining::sparse();
                Ok(())
            }),
            libtest_mimic::Trial::test("remaining_network", || {
                remaining::network();
                Ok(())
            }),
            libtest_mimic::Trial::test("remaining_atomic_push", || {
                remaining::atomic_push();
                Ok(())
            }),
            libtest_mimic::Trial::test("remaining_editing", || {
                remaining::editing();
                Ok(())
            }),
            libtest_mimic::Trial::test("remaining_stashing", || {
                remaining::stashing();
                Ok(())
            }),
            libtest_mimic::Trial::test("transport_http", || {
                transport_gaps::http();
                Ok(())
            }),
            libtest_mimic::Trial::test("transport_local_native", || {
                transport_gaps::local_native();
                Ok(())
            }),
            libtest_mimic::Trial::test("transport_ssh", || {
                transport_gaps::ssh();
                Ok(())
            }),
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
