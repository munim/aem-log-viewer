use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match aem_log_analyzer::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            err.exit_code()
        }
    }
}
