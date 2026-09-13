//! Persistent per-user v2 daemon. Supervised by launchd/systemd or started by
//! the client. Windows runs with the interactive user's filesystem permissions.
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1).peekable();
    if args.peek().is_some_and(|arg| arg == "--build-segment") {
        args.next();
        let input = args
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing build input"))?;
        let output = args
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing build output"))?;
        anyhow::ensure!(args.next().is_none(), "unexpected segment worker argument");
        return filex::daemon::builder::run_worker(input.as_ref(), output.as_ref());
    }
    let mut directory = filex::daemon::ipc::default_directory();
    let mut roots = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            println!("Usage: filex-indexd [--user] [--data-dir DIRECTORY] [ROOT ...]");
            return Ok(());
        }
        if arg == "--user" {
            continue;
        }
        if arg == "--data-dir" {
            directory = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("--data-dir needs a path"))?
                .into();
        } else {
            anyhow::ensure!(
                !arg.to_string_lossy().starts_with('-'),
                "unknown daemon option: {}",
                arg.to_string_lossy()
            );
            roots.push(std::path::PathBuf::from(arg));
        }
    }
    if roots.is_empty() {
        if let Some(path) = filex::settings::default_settings_file() {
            let legacy = filex::ingest::default_roots_file();
            if let Ok(settings) = filex::settings::Settings::load(&path, legacy.as_deref()) {
                roots = settings.roots;
            }
        }
        if roots.is_empty() {
            roots = filex::drives::default_index_roots();
        }
    }
    let _log = filex::logging::init_in("filex-indexd", None);
    filex::telemetry::install_panic_hook("filex-indexd");
    filex::daemon::server::run(
        &directory,
        roots,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
}
