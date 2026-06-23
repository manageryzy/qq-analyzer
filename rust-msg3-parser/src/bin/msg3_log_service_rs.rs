fn main() {
    if let Err(err) = msg3_richtext_parser_rs::msg3_log_service::run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
