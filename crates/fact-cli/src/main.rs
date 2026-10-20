fn main() {
    match fact_cli::run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("fact: {error:#}");
            std::process::exit(1);
        }
    }
}
