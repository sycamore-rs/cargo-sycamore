//! Small, process-wide CLI diagnostics. Command and compiler output stay separate.

use std::{
    fmt,
    io::{self, IsTerminal, Write},
    sync::{
        OnceLock,
        atomic::{AtomicU8, Ordering},
    },
};

use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};

#[derive(Clone, Copy)]
#[repr(u8)]
pub enum Verbosity {
    Quiet,
    Normal,
    Verbose,
}

static VERBOSITY: AtomicU8 = AtomicU8::new(Verbosity::Normal as u8);
static STDERR: OnceLock<StandardStream> = OnceLock::new();

/// Configure tool diagnostics once at startup, before starting worker threads.
pub fn set_verbosity(verbosity: Verbosity) {
    VERBOSITY.store(verbosity as u8, Ordering::Relaxed);
}

fn stderr() -> &'static StandardStream {
    STDERR.get_or_init(|| {
        let choice = if io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
            ColorChoice::Auto
        } else {
            ColorChoice::Never
        };
        StandardStream::stderr(choice)
    })
}

/// Print a Cargo-style status unless quiet mode is enabled.
pub fn status(action: &str, message: fmt::Arguments<'_>) {
    if VERBOSITY.load(Ordering::Relaxed) != Verbosity::Quiet as u8 {
        let color = match action {
            "Serving" | "Watching" | "Changed" => Color::Cyan,
            _ => Color::Green,
        };
        emit(format_args!("{action:>12}"), color, message);
    }
}

pub fn warn(message: fmt::Arguments<'_>) {
    emit(format_args!("warning:"), Color::Yellow, message);
}

pub fn error(message: fmt::Arguments<'_>) {
    emit(format_args!("error:"), Color::Red, message);
}

pub fn debug(message: fmt::Arguments<'_>) {
    if VERBOSITY.load(Ordering::Relaxed) == Verbosity::Verbose as u8 {
        emit(format_args!("{:>12}", "Debug"), Color::Ansi256(8), message);
    }
}

fn emit(label: fmt::Arguments<'_>, color: Color, message: fmt::Arguments<'_>) {
    // Hold the stream for the whole message, including multiline error chains.
    // Diagnostic write failures (e.g. a closed pipe) must not panic or replace
    // the actual command's result.
    let mut stream = stderr().lock();
    let _ = stream.set_color(ColorSpec::new().set_fg(Some(color)).set_bold(true));
    let _ = write!(stream, "{label}");
    let _ = stream.reset();
    let _ = writeln!(stream, " {message}");
}

/// Forward compiler text without formatting or verbosity filtering.
pub(crate) fn raw(message: fmt::Arguments<'_>) {
    let _ = writeln!(stderr().lock(), "{message}");
}
