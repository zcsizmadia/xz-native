//! The .xz container format.

pub(crate) mod flat;
pub(crate) mod format;
#[cfg(feature = "std")]
pub(crate) mod mt;
pub(crate) mod stream;
