// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

use crate::{knowledge_store, registry::RepoEntry};

pub(crate) enum SearchOutcome {
    Lexical(knowledge_store::SearchPage),
    #[cfg(feature = "semantic-knowledge")]
    Semantic {
        hits: Vec<crate::knowledge_vector::SemanticHit>,
        generation: u64,
        candidates_returned: usize,
    },
    Fallback {
        page: knowledge_store::SearchPage,
        reason: &'static str,
    },
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn search(
    conn: &mut rusqlite::Connection,
    home: &Path,
    repo: &RepoEntry,
    query: &str,
    path: Option<&str>,
    kind: Option<&str>,
    include_archived: bool,
    include_superseded: bool,
    include_retracted: bool,
    limit: usize,
    offset: usize,
    generation: Option<u64>,
    semantic: bool,
    model_dir: Option<&Path>,
) -> Result<SearchOutcome, String> {
    if !semantic {
        return knowledge_store::search(
            conn,
            query,
            path,
            kind,
            include_archived,
            include_superseded,
            include_retracted,
            limit,
            offset,
            generation,
        )
        .map(SearchOutcome::Lexical);
    }

    validate_semantic_request(query, path, kind, limit, offset)?;

    #[cfg(feature = "semantic-knowledge")]
    {
        let attempt = crate::knowledge_vector::semantic_search(
            conn, home, repo, query, path, kind, limit, generation, model_dir,
        )?;
        match attempt {
            Ok(page) => Ok(SearchOutcome::Semantic {
                hits: page.hits,
                generation: page.generation,
                candidates_returned: page.candidates_returned,
            }),
            Err(reason) => knowledge_store::search(
                conn,
                query,
                path,
                kind,
                include_archived,
                include_superseded,
                include_retracted,
                limit,
                offset,
                generation,
            )
            .map(|page| SearchOutcome::Fallback { page, reason }),
        }
    }

    #[cfg(not(feature = "semantic-knowledge"))]
    {
        let _ = (home, repo, model_dir);
        knowledge_store::search(
            conn,
            query,
            path,
            kind,
            include_archived,
            include_superseded,
            include_retracted,
            limit,
            offset,
            generation,
        )
        .map(|page| SearchOutcome::Fallback {
            page,
            reason: "feature-unavailable",
        })
    }
}

fn validate_semantic_request(
    query: &str,
    path: Option<&str>,
    kind: Option<&str>,
    limit: usize,
    offset: usize,
) -> Result<(), String> {
    if query.trim().is_empty() {
        return Err("semantic search requires a nonempty text query".into());
    }
    if let Some(path) = path {
        crate::knowledge::validate_search_path(path)?;
    }
    if let Some(kind) = kind {
        crate::knowledge::validate_kind(kind)?;
    }
    if limit > crate::knowledge::SEARCH_LIMIT_MAX {
        return Err(format!(
            "search limit {limit} exceeds maximum {}",
            crate::knowledge::SEARCH_LIMIT_MAX
        ));
    }
    if limit == 0 {
        return Err("search limit must be at least 1".into());
    }
    i64::try_from(limit).map_err(|_| format!("search limit out of range: {limit}"))?;
    if offset != 0 {
        return Err("semantic search does not support a nonzero offset".into());
    }
    if query.len() > crate::knowledge::MAX_QUERY_TEXT_BYTES {
        return Err(format!(
            "search query is {} bytes, at most {} allowed",
            query.len(),
            crate::knowledge::MAX_QUERY_TEXT_BYTES
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_semantic_request;
    use crate::knowledge::{MAX_QUERY_TEXT_BYTES, SEARCH_LIMIT_MAX};

    #[test]
    fn semantic_request_validation_matches_lexical_bounds() {
        assert!(validate_semantic_request("  ", None, None, 10, 0).is_err());
        assert!(validate_semantic_request("x", Some(""), None, 10, 0).is_err());
        assert!(validate_semantic_request("x", None, Some("bad kind"), 10, 0).is_err());
        assert!(validate_semantic_request("x", None, None, 0, 0).is_err());
        assert!(validate_semantic_request("x", None, None, SEARCH_LIMIT_MAX + 1, 0).is_err());
        assert!(validate_semantic_request("x", None, None, 10, 1).is_err());
        let oversized = "x".repeat(MAX_QUERY_TEXT_BYTES + 1);
        assert!(validate_semantic_request(&oversized, None, None, 10, 0).is_err());
        assert!(validate_semantic_request("x", None, None, 10, 0).is_ok());
    }
}
