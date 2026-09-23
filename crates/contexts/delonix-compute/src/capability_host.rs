//! One probe every provider report reuses: is a tool on `PATH`. Kept in the
//! context so the four adapters that build a report do not each keep a copy
//! (the fourth copy is where the shapes start to differ).

/// `true` when `name` is a regular file in one of `PATH`'s directories.
/// Never executes it.
pub fn binary_in_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|p| p.join(name).is_file()))
        .unwrap_or(false)
}
