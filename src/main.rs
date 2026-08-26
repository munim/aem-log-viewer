mod app;

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match app::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            err.exit_code()
        }
    }
}
