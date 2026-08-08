use std::{fmt, io::Write};

use miette::Diagnostic;

pub(crate) fn status(message: impl fmt::Display) {
    println!("{message}");
}

pub(crate) fn note(message: impl fmt::Display) {
    eprintln!("{message}");
}

pub(crate) fn blank_note_line() {
    eprintln!();
}

pub(crate) fn prompt(message: impl fmt::Display) {
    try_prompt(message).expect("failed to flush prompt");
}

pub(crate) fn try_prompt(message: impl fmt::Display) -> std::io::Result<()> {
    eprint!("{message}");
    std::io::stderr().flush()
}

pub(crate) fn warning(diagnostic: impl Diagnostic + Send + Sync + 'static) {
    eprintln!("{:?}", miette::Report::new(diagnostic));
}
