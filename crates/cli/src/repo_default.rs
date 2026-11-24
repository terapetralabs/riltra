// SPDX-License-Identifier: Apache-2.0
//! Default repository for MCP calls that omit `repo`.
//!
//! Only an empty `repo` is resolved: one registered repository fills it, none
//! or several is an error that says what to do. A named `repo` is returned
//! untouched, so a wrong name still meets the existing `unknown repository`
//! path.

use crate::registry::Registry;

/// `Ok(None)`: `repo` is named, use it unchanged. `Ok(Some(name))`: filled
/// from the single registered entry. `Err((kind, message))`: no entry, or
/// more than one.
pub(crate) fn resolve(
    reg: &Registry,
    repo: &str,
) -> Result<Option<String>, (&'static str, String)> {
    if !repo.is_empty() {
        return Ok(None);
    }
    match reg.repos.as_slice() {
        [only] => Ok(Some(only.name.clone())),
        [] => Err(("not_registered", "no repository is registered".to_string())),
        many => {
            let names: Vec<&str> = many.iter().map(|e| e.name.as_str()).collect();
            Err((
                "invalid_input",
                format!(
                    "`repo` is required when more than one repository is registered; registered: {}",
                    names.join(", ")
                ),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{add, Registry};

    fn reg(names: &[&str]) -> Registry {
        let mut reg = Registry {
            version: 1,
            repos: Vec::new(),
        };
        for (i, name) in names.iter().enumerate() {
            add(&mut reg, name, &format!("root-{i}"), 0).unwrap();
        }
        reg
    }

    #[test]
    fn named_repo_is_kept() {
        assert_eq!(resolve(&reg(&["a"]), "b"), Ok(None));
    }

    #[test]
    fn empty_with_one_resolves() {
        assert_eq!(resolve(&reg(&["ws"]), ""), Ok(Some("ws".to_string())));
    }

    #[test]
    fn empty_with_none_is_not_registered() {
        assert_eq!(
            resolve(&reg(&[]), ""),
            Err(("not_registered", "no repository is registered".to_string()))
        );
    }

    #[test]
    fn empty_with_two_lists_names() {
        assert_eq!(
            resolve(&reg(&["a", "b"]), ""),
            Err((
                "invalid_input",
                "`repo` is required when more than one repository is registered; registered: a, b"
                    .to_string()
            ))
        );
    }

    #[test]
    fn whitespace_is_not_empty() {
        assert_eq!(resolve(&reg(&["ws"]), " "), Ok(None));
    }
}
