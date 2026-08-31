use std::io::IsTerminal;

fn main() {
    std::process::exit(arsy_cli::run_cli(
        std::env::args().skip(1),
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
    ));
}
