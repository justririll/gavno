//! Root-side helper of gavno-files. Started by the file manager through
//! `pkexec`; speaks the line protocol in `root_proto.rs` on stdin/stdout and
//! exits when the file manager closes its stdin.
#[path = "../root_proto.rs"]
mod root_proto;

fn main() -> std::process::ExitCode {
    if std::env::var_os("PKEXEC_UID").is_none() {
        eprintln!("gavno-files-helper is started by gavno-files through pkexec");
        return std::process::ExitCode::from(2);
    }
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    match root_proto::serve(stdin.lock(), stdout.lock()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("gavno-files-helper: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
