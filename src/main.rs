fn main() {
    std::process::exit(cachekit_cli::main(std::env::args_os().skip(1)));
}
