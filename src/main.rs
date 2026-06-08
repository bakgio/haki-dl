#![deny(unsafe_code)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]
#![deny(clippy::unwrap_used)]

#[tokio::main]
async fn main() -> std::process::ExitCode {
    Box::pin(haki_dl::cli::run_cli(std::env::args().skip(1))).await
}
