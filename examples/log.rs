use std::{env, fs::File};
use tracing::{debug, error, info};
use tracing_log::LogTracer;
use tracing_subscriber::{EnvFilter, fmt::writer::MakeWriterExt};

fn main() {
    // 0) Capture `log::*` records and forward them into `tracing`
    //    (so log::info! will appear alongside tracing::info!)
    LogTracer::init().ok();

    // 1) Open (or create) the file sink
    let file = File::options()
        .create(true)
        .append(true)
        .open("tracing.log")
        .expect("open tracing.log");

    // 2) Build subscriber that reads RUST_LOG and writes to file
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info")) // default if RUST_LOG unset
        )
        .with_target(true)                    // show module/target
        .with_ansi(false)                     // plain text in file
        .with_writer(file.with_max_level(tracing::Level::TRACE)) // require MakeWriterExt
        .finish();

    tracing::subscriber::set_global_default(subscriber)
        .expect("set global subscriber");

    // 3) Test logs via tracing macros
    info!("App started (tracing)");
    debug!("Debug log activated (tracing)");
    error!("Error log activated (tracing)");

    // 4) Test logs via log macros (exercising the bridge)
    log::info!("App started (log)");
    log::debug!("Debug log activated (log)");

    // Your code:
    let _fifo_path = env::var("PYREPL_FIFO").unwrap_or_else(|_| "default_fifo".to_string());
}

