//! Real Git/OpenSSH interoperability tests. This executable also serves as the
//! temporary sshd's forced command, keeping the entire fixture in Rust.
#[path = "suites/bootstrap.rs"]
mod bootstrap;
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
