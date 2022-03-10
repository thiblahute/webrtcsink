use async_std::task;
use clap::Parser;
use tracing_subscriber::prelude::*;
use webrtcsink_signalling::handlers::DefaultMessageHandler;
use webrtcsink_signalling::server::{SignallingServer, SignallingServerError};

#[derive(Parser, Debug)]
#[clap(about, version, author)]
/// Program arguments
struct Args {
    /// Address to listen on
    #[clap(short, long, default_value = "0.0.0.0")]
    host: String,
    /// Port to listen on
    #[clap(short, long, default_value_t = 8443)]
    port: u16,

    #[clap(short, long, help = "TLS certificate to use.")]
    cert: Option<String>,

    #[clap(long, help = "TLS certificate to password.")]
    password: Option<String>,
}

fn main() -> Result<(), SignallingServerError> {
    let args = Args::parse();
    let server = SignallingServer::new(
        Box::new(DefaultMessageHandler::default()),
        args.cert.clone(),
        args.password.clone(),
    );

    tracing_log::LogTracer::init().expect("Failed to set logger");
    let env_filter =
        tracing_subscriber::EnvFilter::try_from_env("WEBRTCSINK_SIGNALLING_SERVER_LOG")
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_thread_ids(true)
        .with_target(true)
        .with_span_events(
            tracing_subscriber::fmt::format::FmtSpan::NEW
                | tracing_subscriber::fmt::format::FmtSpan::CLOSE,
        );
    let subscriber = tracing_subscriber::Registry::default()
        .with(env_filter)
        .with(fmt_layer);
    tracing::subscriber::set_global_default(subscriber).expect("Failed to set subscriber");

    task::block_on(server.run(&args.host, args.port))
}
