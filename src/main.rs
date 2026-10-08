#[expect(clippy::print_stderr, reason = "the top-level error report")]
fn main() -> std::process::ExitCode {
    match anipv::cli::main() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        // `anipv ls | head` closes the pipe early; that's not an error.
        Err(e)
            if e.chain().any(|c| {
                c.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
            }) =>
        {
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("\x1b[31merror:\x1b[0m {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
