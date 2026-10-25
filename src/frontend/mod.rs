//! The front files the app reads from disk at launch instead of carrying them in the binary.

pub(crate) mod dir;
/// `files.rs` as the app reads it: one constant per file, which `load` reads by name.
macro_rules! emit_top_files {
    ($($name:ident = $file:literal,)*) => {
        $(pub(crate) const $name: &str = $file;)*
    };
}
pub(crate) mod files;
pub(crate) mod load;
pub(crate) mod prologue;

#[cfg(test)]
#[path = "../../tests/unit/frontend/fixture.rs"]
pub(crate) mod fixture;
