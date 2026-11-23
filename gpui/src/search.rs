//! Shared fuzzy search for every GPUI search field.
//!
//! One [`SkimMatcherV2`] scores every whitespace-separated query term against a
//! weighted list of fields. A row matches only when *all* terms match at least
//! one field, so `triage favetto` finds a row whose label has one term and whose
//! prompt has the other. The field weights establish the relative hierarchy
//! (label > namespace > body > metadata) while the matcher's own score still
//! orders rows inside one field.
//!
//! The Catalog, Tasks, command palette, repo picker and slash completion all
//! use this module so their normalization, term rules and case folding cannot
//! drift apart.

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;

/// Relative weights for the search fields. The gaps are wider than the raw
/// matcher's score range, so a label hit always outranks a body-only or
/// metadata-only hit.
pub mod weight {
    /// A visible row label, task name or repository name.
    pub const LABEL: i64 = 10_000;
    /// A visible namespace, folder or repository path.
    pub const NAMESPACE: i64 = 5_000;
    /// Prompt/body/description text.
    pub const BODY: i64 = 2_500;
    /// Agent, model, kind, var names and other metadata.
    pub const META: i64 = 1_000;
}

/// The matcher shared by every search field. Case-insensitive matching keeps
/// slash completion usable with a capitalised query and makes short queries
/// behave the same everywhere.
pub fn matcher() -> SkimMatcherV2 {
    SkimMatcherV2::default().ignore_case()
}

/// One searchable haystack plus its relative weight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchField<'a> {
    pub text: &'a str,
    pub weight: i64,
}

impl<'a> SearchField<'a> {
    pub fn new(text: &'a str, weight: i64) -> Self {
        Self { text, weight }
    }
}

/// A normalized query: trimmed and split on whitespace, parsed once per pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchQuery<'a> {
    terms: Vec<&'a str>,
}

impl<'a> SearchQuery<'a> {
    pub fn new(raw: &'a str) -> Self {
        Self {
            terms: raw.split_whitespace().collect(),
        }
    }

    /// Whether the query has no terms (whitespace-only counts as empty).
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Score `fields` against every term. Returns `None` unless every term
    /// matches at least one field. Fields are tried independently for each term,
    /// so terms may land on different fields in any order.
    pub fn score(
        &self,
        matcher: &SkimMatcherV2,
        fields: &[SearchField<'_>],
    ) -> Option<SearchMatch> {
        if self.terms.is_empty() {
            return Some(SearchMatch::default());
        }

        let mut score = 0i64;
        let mut matched_fields = 0u64;
        for term in &self.terms {
            let mut best: Option<(i64, usize)> = None;
            for (index, field) in fields.iter().enumerate() {
                let Some(raw) = matcher.fuzzy_match(field.text, term) else {
                    continue;
                };
                let candidate = raw.saturating_add(field.weight);
                if best.is_none_or(|(best_score, _)| candidate > best_score) {
                    best = Some((candidate, index));
                }
            }
            let (best_score, index) = best?;
            score = score.saturating_add(best_score);
            if index < u64::BITS as usize {
                matched_fields |= 1u64 << index;
            }
        }

        Some(SearchMatch {
            score,
            matched_fields,
        })
    }
}

/// A successful match: its relevance score and which fields were hit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchMatch {
    /// Higher is more relevant. Terms are summed, so matching several terms
    /// sorts above a single-term hit.
    pub score: i64,
    /// Bit `i` is set when field `i` matched at least one term.
    pub matched_fields: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_term_must_match_and_terms_can_hit_different_fields() {
        let matcher = matcher();
        let fields = [
            SearchField::new("gpui/01-scaffold", weight::LABEL),
            SearchField::new("wire the transport", weight::BODY),
        ];

        let both = SearchQuery::new("scaffold transport");
        let hit = both.score(&matcher, &fields).expect("both terms match");
        assert_ne!(hit.matched_fields & 0b01, 0);
        assert_ne!(hit.matched_fields & 0b10, 0);

        assert!(SearchQuery::new("scaffold missing")
            .score(&matcher, &fields)
            .is_none());

        let reordered = SearchQuery::new("transport scaffold");
        assert!(reordered.score(&matcher, &fields).is_some());
    }

    #[test]
    fn labels_outrank_body_and_metadata_hits() {
        let matcher = matcher();
        let label = SearchQuery::new("release")
            .score(&matcher, &[SearchField::new("release", weight::LABEL)])
            .unwrap();
        let body = SearchQuery::new("release")
            .score(
                &matcher,
                &[SearchField::new("publish a release", weight::BODY)],
            )
            .unwrap();
        let meta = SearchQuery::new("release")
            .score(&matcher, &[SearchField::new("release", weight::META)])
            .unwrap();
        assert!(label.score > body.score);
        assert!(body.score > meta.score);
    }

    #[test]
    fn whitespace_is_trimmed_and_an_empty_query_matches_every_row() {
        let matcher = matcher();
        let query = SearchQuery::new("   ");
        assert!(query.is_empty());
        assert_eq!(
            query.score(&matcher, &[SearchField::new("anything", weight::LABEL)]),
            Some(SearchMatch::default())
        );

        let spaced = SearchQuery::new("  release   notes ");
        assert_eq!(spaced.terms, vec!["release", "notes"]);
    }

    #[test]
    fn matching_is_case_insensitive() {
        let matcher = matcher();
        assert!(SearchQuery::new("RELEASE")
            .score(
                &matcher,
                &[SearchField::new("release notes", weight::LABEL)]
            )
            .is_some());
    }

    #[test]
    fn matching_several_terms_scores_higher_than_one() {
        let matcher = matcher();
        let fields = [
            SearchField::new("triage favetto", weight::LABEL),
            SearchField::new("sort the inbox", weight::BODY),
        ];
        let one = SearchQuery::new("triage").score(&matcher, &fields).unwrap();
        let two = SearchQuery::new("triage favetto")
            .score(&matcher, &fields)
            .unwrap();
        assert!(two.score > one.score);
    }
}
