use std::io;

fn main() {
    if std::env::args().skip(1).collect::<Vec<_>>() != ["mcp"] {
        eprintln!("usage: ds-agent mcp");
        std::process::exit(2);
    }
    if let Err(error) = ds_agent::serve(io::stdin().lock(), io::stdout().lock(), ds_agent::forward)
    {
        eprintln!("ds-agent: {error}");
        std::process::exit(1);
    }
}
