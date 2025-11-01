use std::io::stdout;

use tracing::level_filters::LevelFilter;
use tracing_subscriber::{
    EnvFilter, Layer,
    fmt::{self},
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

pub fn register_tracing_subscriber(quiet: bool) {
    let stdout_layer = fmt::layer()
        .pretty()
        .with_writer(stdout)
        .with_filter(EnvFilter::from_default_env());

    let log_path = crate::path::logs_path();
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .expect("Failed to open log file");
    let json_layer = fmt::layer()
        .json()
        .with_writer(log_file)
        .with_filter(LevelFilter::DEBUG);

    if !quiet {
        tracing_subscriber::registry()
            .with(json_layer)
            .with(stdout_layer)
            .init();
    } else {
        tracing_subscriber::registry().with(json_layer).init();
    }
}
