fn main() {
    match aem_log_analyzer::perf::run_release() {
        Ok(()) => {}
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }
}
