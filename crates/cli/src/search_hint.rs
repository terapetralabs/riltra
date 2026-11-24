// SPDX-License-Identifier: Apache-2.0
//! Empty-result hint for symbol search: the shape of the query that matched
//! no definition name, so the document can point at `query` or text search.
// Task 3 removes this attribute when the CLI and MCP call into the module.
#![cfg_attr(not(test), allow(dead_code))]

/// The `empty` hint carried by an empty search document. Only `query_shape`
/// varies; the other keys of the `empty` object are constants.
pub(crate) struct EmptyHint {
    pub query_shape: &'static str,
}

/// `"identifier"` when every character of the trimmed query is ASCII
/// alphanumeric or one of `_ . : $ -`; otherwise `"text"` (inner spaces and
/// non-ASCII letters included). An empty trimmed query returns `"text"`; the
/// callers never build a hint for one.
pub(crate) fn query_shape(query: &str) -> &'static str {
    let trimmed = query.trim();
    if !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '$' | '-'))
    {
        "identifier"
    } else {
        "text"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_shape_classifies() {
        assert_eq!(query_shape("matchPath"), "identifier");
        assert_eq!(query_shape("crate::store"), "identifier");
        assert_eq!(query_shape("Expected component"), "text");
        assert_eq!(query_shape("Überblick"), "text");
        assert_eq!(query_shape("naïve"), "text");
        assert_eq!(query_shape("  matchPath  "), "identifier");
        assert_eq!(query_shape("$scope-id"), "identifier");
        assert_eq!(query_shape(""), "text");
    }
}
