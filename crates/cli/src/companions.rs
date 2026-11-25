// SPDX-License-Identifier: Apache-2.0
//! Paired files for a grep hit (`companions` in the grep document). The
//! functions that fill them arrive with the companion matchers; this module
//! holds the shared shape that the grep document already renders.

/// One paired file: why it was named, on what basis, and through which name.
// shortcut: no producer yet; the companion matchers construct it.
#[allow(dead_code)]
pub(crate) struct Companion {
    pub path: String,
    pub why: &'static str,
    pub basis: &'static str,
    pub via: String,
}
