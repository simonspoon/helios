/// Error indicating no helios index exists in the current directory.
/// Commands that require an index return this so main.rs can exit with code 2.
#[derive(Debug)]
pub struct NoIndexError;

impl std::fmt::Display for NoIndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "No index found. Run `helios init` first.")
    }
}

impl std::error::Error for NoIndexError {}
