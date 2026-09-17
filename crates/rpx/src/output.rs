use std::{fmt, io::Write};

pub(crate) fn status(message: impl fmt::Display) {
    println!("{message}");
}

pub(crate) fn try_prompt(message: impl fmt::Display) -> std::io::Result<()> {
    eprint!("{message}");
    std::io::stderr().flush()
}
