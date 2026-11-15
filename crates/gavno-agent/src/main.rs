use std::io;

fn main() {
    if std::env::args().skip(1).collect::<Vec<_>>() != ["mcp"] {
        eprintln!("usage: gavno-agent mcp");
        std::process::exit(2);
    }
    if let Err(error) = gavno_agent::serve(io::stdin().lock(), io::stdout().lock(), gavno_agent::forward)
    {
        eprintln!("gavno-agent: {error}");
        std::process::exit(1);
    }
}
