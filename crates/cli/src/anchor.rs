// SPDX-License-Identifier: Apache-2.0
//! Which stored occurrence answers for a source position.
//!
//! A producer may give a module a definition occurrence as wide as its file.
//! Such an occurrence holds every definition in it, so it is not "at" a
//! position inside one of them: that definition is what was asked about.

use crate::store::{self, SemanticFact, SymbolRow};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::rc::Rc;

use crate::syntax::{classify, language_name, parse_source, FileKind};

const DEF_ROLE: u32 = 0x1;

/// A position in source that is byte-identical to the indexed file.
pub(crate) struct Position<'a> {
    pub path: &'a str,
    pub source: &'a [u8],
    pub offset: usize,
    pub defs: &'a [SymbolRow],
}

/// The narrowest definitions holding `offset`; more than one only on equal width.
pub(crate) fn definitions_at(defs: &[SymbolRow], offset: usize) -> Vec<&SymbolRow> {
    narrowest(
        defs.iter()
            .filter(|d| d.byte_start <= offset && offset < d.byte_end),
        |d| d.byte_end.saturating_sub(d.byte_start),
    )
}

/// The unqualified part of a stored definition name (after the last `::` or `.`).
pub(crate) fn short_name(definition: &SymbolRow) -> Option<&str> {
    let name = definition.name.as_deref()?;
    let last = name.rsplit("::").next().unwrap_or(name);
    Some(last.rsplit('.').next().unwrap_or(last)).filter(|n| !n.is_empty())
}

/// What `syntax::FileFacts::identifiers` holds for one file.
type Identifiers = Rc<Vec<(usize, usize, usize, usize)>>;

#[derive(Default)]
struct Parsed {
    holders: usize,
    files: BTreeMap<(&'static str, String), Identifiers>,
}

thread_local! {
    static PARSED: RefCell<Parsed> = RefCell::default();
}

/// Held for one operation, which runs on one thread: a file is parsed once
/// however many of its anchors the operation looks at. Dropping the last holder
/// forgets everything, so nothing outlives the operation.
pub(crate) struct ParseOnce(());

impl ParseOnce {
    pub(crate) fn hold() -> Self {
        PARSED.with(|parsed| parsed.borrow_mut().holders += 1);
        Self(())
    }
}

impl Drop for ParseOnce {
    fn drop(&mut self) {
        PARSED.with(|parsed| {
            let mut parsed = parsed.borrow_mut();
            parsed.holders -= 1;
            if parsed.holders == 0 {
                parsed.files.clear();
            }
        });
    }
}

fn identifiers(path: &str, source: &[u8]) -> Identifiers {
    let FileKind::Supported(language) = classify(Path::new(path)) else {
        return Rc::default();
    };
    let key = (language_name(language), store::content_digest(source));
    if let Some(known) = PARSED.with(|parsed| parsed.borrow().files.get(&key).cloned()) {
        return known;
    }
    let found: Identifiers = Rc::new(parse_source(language, source).identifiers);
    PARSED.with(|parsed| {
        let mut parsed = parsed.borrow_mut();
        if parsed.holders > 0 {
            parsed.files.insert(key, found.clone());
        }
    });
    found
}

/// The definition occurrences that own `definition`: they stand on the
/// identifier it declares, and in no definition nested in it. The same text
/// elsewhere in it (a local, a nested item) proves nothing. The flag says that
/// a same-named occurrence sits in a nested definition. `facts` and `source`
/// must be the current bytes of `path`.
pub(crate) fn owned<'a>(
    facts: impl IntoIterator<Item = &'a SemanticFact>,
    defs: &[SymbolRow],
    definition: &SymbolRow,
    source: &[u8],
    path: &str,
) -> (Vec<&'a SemanticFact>, bool) {
    let mut found = Vec::new();
    let mut nested_same_name = false;
    let Some(name) = short_name(definition) else {
        return (found, nested_same_name);
    };
    let width = definition.byte_end.saturating_sub(definition.byte_start);
    let mut declared: Option<Identifiers> = None;
    for fact in facts {
        let own = fact.roles & DEF_ROLE != 0
            && fact.symbol_uri.is_some()
            && source.get(fact.byte_start..fact.byte_end) == Some(name.as_bytes())
            && definition.byte_start <= fact.byte_start
            && fact.byte_end <= definition.byte_end;
        if !own {
            continue;
        }
        let nested = defs.iter().any(|d| {
            definition.byte_start <= d.byte_start
                && d.byte_end <= definition.byte_end
                && d.byte_end.saturating_sub(d.byte_start) < width
                && d.byte_start <= fact.byte_start
                && fact.byte_end <= d.byte_end
        });
        if nested {
            nested_same_name = true;
        } else if declared
            .get_or_insert_with(|| identifiers(path, source))
            .contains(&(
                definition.byte_start,
                definition.byte_end,
                fact.byte_start,
                fact.byte_end,
            ))
        {
            found.push(fact);
        }
    }
    (found, nested_same_name)
}

/// The narrowest occurrences answering a position. `containing` holds the facts
/// whose span holds it; `usable` all facts of the file a caller accepts. With a
/// `position`, an occurrence wider than the definition there and holding all of
/// it is passed over; when nothing else is at the position, the definition's own
/// occurrence answers, and none (not exactly one place) answers nothing.
/// Without one (no current source) the narrowest containing fact answers.
pub(crate) fn select<'a>(
    containing: Vec<&'a SemanticFact>,
    usable: impl IntoIterator<Item = &'a SemanticFact>,
    position: Option<&Position<'_>>,
) -> Vec<&'a SemanticFact> {
    let at = position.map(|p| (p, definitions_at(p.defs, p.offset)));
    let definition = at.as_ref().and_then(|(_, found)| found.first().copied());
    let (wider, mut rest): (Vec<_>, Vec<_>) = containing
        .into_iter()
        .partition(|fact| definition.is_some_and(|d| encloses(fact, d)));
    if rest.is_empty() && !wider.is_empty() {
        if let Some((p, found)) = &at {
            // Overlapping definitions stay for the syntax fallback to refuse.
            if let [only] = found[..] {
                let (own, _) = owned(usable, p.defs, only, p.source, p.path);
                let places: BTreeSet<_> = own.iter().map(|f| (f.byte_start, f.byte_end)).collect();
                if places.len() == 1 {
                    rest = own;
                }
            }
        }
    }
    narrowest(rest, |f| f.byte_end.saturating_sub(f.byte_start))
}

fn encloses(fact: &SemanticFact, definition: &SymbolRow) -> bool {
    fact.byte_start <= definition.byte_start
        && definition.byte_end <= fact.byte_end
        && fact.byte_end - fact.byte_start > definition.byte_end - definition.byte_start
}

/// The items of the smallest width, in their given order.
fn narrowest<T>(items: impl IntoIterator<Item = T>, width: impl Fn(&T) -> usize) -> Vec<T> {
    let mut best = Vec::new();
    let mut smallest = usize::MAX;
    for item in items {
        let size = width(&item);
        if size < smallest {
            smallest = size;
            best.clear();
        }
        if size == smallest {
            best.push(item);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_parsed_once_per_operation_and_forgotten_after_it() {
        let source = b"fn f() {}\n";
        let parsed = || identifiers("a.rs", source);
        assert!(!Rc::ptr_eq(&parsed(), &parsed()), "nothing is kept outside");
        let first = {
            let _outer = ParseOnce::hold();
            let first = parsed();
            {
                let _inner = ParseOnce::hold();
                assert!(Rc::ptr_eq(&first, &parsed()));
            }
            assert!(Rc::ptr_eq(&first, &parsed()), "an inner holder keeps it");
            first
        };
        let _next = ParseOnce::hold();
        assert!(!Rc::ptr_eq(&first, &parsed()), "the last holder forgets");
        assert_eq!(*first, vec![(0, 9, 3, 4)]);
    }
}
