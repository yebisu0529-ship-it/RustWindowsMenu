use std::fmt;

/// A failure the user has to read. `main` prints it and exits non-zero.
#[derive(Debug)]
pub struct CliError(String);

pub type CliResult<T> = Result<T, CliError>;

impl CliError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// `CliError` deliberately does not implement `std::error::Error`, which is what lets this
// blanket conversion exist and `?` work on every library error.
impl<E: std::error::Error> From<E> for CliError {
    fn from(err: E) -> Self {
        Self(err.to_string())
    }
}

pub trait Context<T> {
    /// Prefixes the error with what the program was doing when it happened.
    fn context(self, what: impl FnOnce() -> String) -> CliResult<T>;
}

impl<T, E: fmt::Display> Context<T> for Result<T, E> {
    fn context(self, what: impl FnOnce() -> String) -> CliResult<T> {
        self.map_err(|err| CliError(format!("{}: {err}", what())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_prefixes_the_underlying_error() {
        let failed: Result<(), std::io::Error> = Err(std::io::Error::other("disk full"));
        let err = failed
            .context(|| "copying rwm.exe".to_string())
            .unwrap_err();
        assert_eq!(err.to_string(), "copying rwm.exe: disk full");
    }

    #[test]
    fn question_mark_converts_library_errors() {
        fn parse() -> CliResult<u32> {
            Ok("not a number".parse::<u32>()?)
        }
        assert!(parse().unwrap_err().to_string().contains("invalid digit"));
    }
}
