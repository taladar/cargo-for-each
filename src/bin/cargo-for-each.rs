#![doc = include_str!("../../README.md")]

use tracing_subscriber::{
    EnvFilter, Layer as _, Registry, filter::LevelFilter, layer::SubscriberExt as _,
    util::SubscriberInitExt as _,
};

/// The main behavior of the binary should go here
///
/// # Errors
///
/// fails if the main behavior of the application fails
async fn do_stuff() -> Result<(), cargo_for_each::error::Error> {
    let options = <cargo_for_each::Options as clap::Parser>::parse();
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
    //use super::*;
    //use pretty_assertions::{assert_eq, assert_ne};
}
