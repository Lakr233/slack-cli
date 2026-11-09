use std::io::{self, IsTerminal, Write};

fn main() {
    // Decoding recurses once per nesting level of the Redux store.
    let code = std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(run)
        .expect("start main thread")
        .join()
        .unwrap_or(101);
    std::process::exit(code);
}

fn run() -> i32 {
    let stdout = io::stdout();
    let tty = stdout.is_terminal();
    let width = terminal_size::terminal_size()
        .map(|(w, _)| usize::from(w.0))
        .or_else(|| std::env::var("COLUMNS").ok()?.parse().ok())
        .unwrap_or(120);
    let color = tty && std::env::var_os("NO_COLOR").is_none();
    let term = slack_cli::Term { tty, width, color };
    let mut out = stdout.lock();
    let mut err = io::stderr().lock();
    let result = slack_cli::run(std::env::args_os(), &mut out, &mut err, &term)
        .and_then(|code| out.flush().map(|()| code));
    match result {
        Ok(code) => code,
        // The reader went away, as with `slack-cli history x | head`.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => 0,
        Err(e) => {
            let _ = writeln!(err, "slack-cli: {e}");
            1
        }
    }
}
