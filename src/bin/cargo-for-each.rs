#![doc = include_str!("../../README.md")]

use std::ffi::{OsStr, OsString};

use tracing_subscriber::{
    EnvFilter, Layer as _, Registry, filter::LevelFilter, layer::SubscriberExt as _,
    util::SubscriberInitExt as _,
};

/// Cargo invokes external subcommands with the subcommand name as `argv[1]`:
/// `cargo for-each foo bar` runs `cargo-for-each for-each foo bar`. If we
/// hand that to clap unchanged, clap sees `for-each` as the first
/// positional/subcommand and errors out. This helper detects that case,
/// drops `argv[1]`, and reports whether it did so — so the caller can
/// override `bin_name` for help/usage text.
///
/// `argv[0]` (the program name) is always preserved; only the literal
/// `argv[1] == "for-each"` is filtered. Later positional values equal to
/// `"for-each"` (e.g. `cargo-for-each task create --name for-each`) are
/// left alone.
fn rewrite_argv_for_cargo_subcommand<I, S>(args: I) -> (bool, Vec<OsString>)
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut iter = args.into_iter().map(Into::into);
    let Some(arg0) = iter.next() else {
        return (false, Vec::new());
    };
    match iter.next() {
        Some(arg1) if arg1 == OsStr::new("for-each") => {
            let mut out: Vec<OsString> = Vec::new();
            out.push(arg0);
            out.extend(iter);
            (true, out)
        }
        Some(arg1) => {
            let mut out: Vec<OsString> = Vec::with_capacity(2);
            out.push(arg0);
            out.push(arg1);
            out.extend(iter);
            (false, out)
        }
        None => (false, vec![arg0]),
    }
}

/// The main behavior of the binary should go here
///
/// # Errors
///
/// fails if the main behavior of the application fails
async fn do_stuff() -> Result<(), cargo_for_each::error::Error> {
    let (is_cargo_subcommand, argv) = rewrite_argv_for_cargo_subcommand(std::env::args_os());
    let mut cmd = <cargo_for_each::Options as clap::CommandFactory>::command();
    if is_cargo_subcommand {
        // Cosmetic: makes `cargo for-each --help` print
        // `Usage: cargo for-each ...` instead of `Usage: cargo-for-each ...`.
        cmd = cmd.bin_name("cargo for-each");
    }
    let matches = cmd.get_matches_from(argv);
    let options = <cargo_for_each::Options as clap::FromArgMatches>::from_arg_matches(&matches)
        .unwrap_or_else(|e| e.exit());
    tracing::debug!("{:#?}", options);

    let environment = cargo_for_each::Environment::new()?;

    cargo_for_each::run_app(options, environment).await
}

/// The main function mainly just handles setting up tracing
/// and handling any Err Results.
#[tokio::main]
async fn main() -> Result<(), cargo_for_each::error::Error> {
    let terminal_env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::WARN.into())
        .parse(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".to_string()))?;
    let file_env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::TRACE.into())
        .parse(std::env::var("CARGO_FOR_EACH_LOG").unwrap_or_else(|_| "trace".to_string()))?;
    #[cfg(target_os = "linux")]
    let journald_env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::TRACE.into())
        .parse(
            std::env::var("CARGO_FOR_EACH_JOURNALD_LOG").unwrap_or_else(|_| "info".to_string()),
        )?;
    let registry = Registry::default();
    let registry =
        registry.with(tracing_subscriber::fmt::Layer::default().with_filter(terminal_env_filter));
    let log_dir = std::env::var("CARGO_FOR_EACH_LOG_DIR");
    let file_layer = if let Ok(log_dir) = log_dir {
        let log_file = if let Ok(log_file) = std::env::var("CARGO_FOR_EACH_LOG_FILE") {
            log_file
        } else {
            "cargo_for_each.log".to_string()
        };
        let file_appender = tracing_appender::rolling::never(log_dir, log_file);
        Some(
            tracing_subscriber::fmt::Layer::default()
                .with_writer(file_appender)
                .with_filter(file_env_filter),
        )
    } else {
        None
    };
    let registry = registry.with(file_layer);
    #[cfg(target_os = "linux")]
    let registry = registry.with(
        tracing_journald::layer()
            .map_err(cargo_for_each::error::Error::TracingJournaldError)?
            .with_filter(journald_env_filter),
    );
    registry.init();
    log_panics::init();
    #[expect(
        clippy::print_stderr,
        reason = "This is the final print in our error chain and we already log this with tracing above but depending on log level the tracing output is not seen by the user"
    )]
    match do_stuff().await {
        Ok(()) => (),
        Err(e) => {
            tracing::error!("{e}");
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
    tracing::debug!("Exiting");
    Ok(())
}

#[cfg(test)]
mod test {
    #![expect(
        clippy::panic,
        reason = "test helpers panic on unexpected inputs that should never appear in fixtures"
    )]

    use super::rewrite_argv_for_cargo_subcommand;
    use pretty_assertions::assert_eq;

    /// Convenience: invoke the helper with `&str` arguments and unwrap the
    /// `OsString` result back to `String` for easy comparison.
    fn rewrite(args: &[&str]) -> (bool, Vec<String>) {
        let (flag, argv) = rewrite_argv_for_cargo_subcommand(args.iter().copied());
        let argv_strings: Vec<String> = argv
            .into_iter()
            .map(|s| {
                s.into_string()
                    .unwrap_or_else(|os| panic!("test args must be UTF-8: {os:?}"))
            })
            .collect();
        (flag, argv_strings)
    }

    #[test]
    fn empty_argv_passes_through_unchanged() {
        let (is_cargo, argv) = rewrite(&[]);
        assert!(!is_cargo);
        assert!(argv.is_empty());
    }

    #[test]
    fn program_name_only_passes_through_unchanged() {
        let (is_cargo, argv) = rewrite(&["cargo-for-each"]);
        assert!(!is_cargo);
        assert_eq!(argv, vec!["cargo-for-each".to_owned()]);
    }

    #[test]
    fn direct_invocation_with_subcommand_passes_through_unchanged() {
        let (is_cargo, argv) = rewrite(&["cargo-for-each", "task", "list"]);
        assert!(!is_cargo);
        assert_eq!(
            argv,
            vec![
                "cargo-for-each".to_owned(),
                "task".to_owned(),
                "list".to_owned()
            ]
        );
    }

    #[test]
    fn cargo_invocation_strips_for_each_and_reports_true() {
        // What cargo gives us when the user runs `cargo for-each task list`.
        let (is_cargo, argv) = rewrite(&["cargo-for-each", "for-each", "task", "list"]);
        assert!(is_cargo);
        assert_eq!(
            argv,
            vec![
                "cargo-for-each".to_owned(),
                "task".to_owned(),
                "list".to_owned()
            ]
        );
    }

    #[test]
    fn for_each_as_value_in_later_position_is_preserved() {
        // The literal "for-each" appears as a `--name` value, not at
        // position 1: must not be stripped.
        let (is_cargo, argv) = rewrite(&["cargo-for-each", "task", "create", "--name", "for-each"]);
        assert!(!is_cargo);
        assert_eq!(
            argv,
            vec![
                "cargo-for-each".to_owned(),
                "task".to_owned(),
                "create".to_owned(),
                "--name".to_owned(),
                "for-each".to_owned(),
            ]
        );
    }

    #[test]
    fn cargo_invocation_with_no_subcommand_after_for_each_still_strips() {
        // `cargo for-each` with no further args: helper still strips the
        // trigger token so clap can produce a proper "missing subcommand"
        // error instead of a confusing "unknown subcommand 'for-each'" one.
        let (is_cargo, argv) = rewrite(&["cargo-for-each", "for-each"]);
        assert!(is_cargo);
        assert_eq!(argv, vec!["cargo-for-each".to_owned()]);
    }
}
