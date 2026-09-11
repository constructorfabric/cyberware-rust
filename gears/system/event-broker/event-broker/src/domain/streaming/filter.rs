//! Whether an event belongs to a subscription.
//!
//! Three predicates and no more: the event's topic equals the interest's topic,
//! its type matches one of the interest's GTS patterns, and its tenant is within
//! the interest's scope. An interest may also carry a `FilterSpec`, but the
//! expression language, the engine contract and the evaluation semantics belong
//! to ADR-0005; this module owns only *where* such an engine is invoked.
//!
//! Compiled once at stream open and validated at JOIN, so a malformed pattern is
//! a `400` at JOIN rather than a stream that silently matches nothing.

use std::collections::HashMap;

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{Event, FilterSpec, Interest};

/// Whether one event is wanted.
pub trait EventFilter: Send + Sync {
    fn matches(&self, event: &Event) -> bool;
}

/// One interest, compiled.
#[derive(Debug)]
struct CompiledInterest {
    tenant_id: Uuid,
    /// Pre-split patterns. Splitting on every event would repeat the same work
    /// once per event per reader, and a saturating filter examines far more
    /// events than it delivers.
    patterns: Vec<Vec<String>>,
    /// Carried, not evaluated, and deliberately unread until ADR-0005 lands.
    ///
    /// This is the seam an engine plugs into. Dropping the field instead would
    /// lose the subscription's declared filter at compile time, so a later
    /// engine would have nothing to evaluate and the JOIN-time value would have
    /// been silently discarded.
    #[expect(
        dead_code,
        reason = "the engine that reads it is ADR-0005's, still proposed"
    )]
    filter: Option<FilterSpec>,
}

/// A subscription's interests, compiled and indexed by topic.
#[derive(Debug)]
pub struct InterestFilter {
    /// Keyed by the topic's string form, because that is what an event carries
    /// and a map lookup beats scanning every interest for every event.
    by_topic: HashMap<String, Vec<CompiledInterest>>,
}

impl InterestFilter {
    /// Compiles a subscription's interests.
    ///
    /// Called twice with different intent: at `JOIN` to validate, discarding the
    /// result, and at stream open to build. One code path, so a subscription
    /// that joined cannot fail to compile later.
    ///
    /// # Errors
    /// [`DomainError::Validation`] with code `BadTypePattern` when a type
    /// pattern breaks the GTS wildcard rules.
    pub fn compile(interests: &[Interest]) -> Result<Self, DomainError> {
        let mut by_topic: HashMap<String, Vec<CompiledInterest>> = HashMap::new();

        for interest in interests {
            let mut patterns = Vec::with_capacity(interest.types.len());
            for pattern in &interest.types {
                validate_type_pattern(pattern)?;
                patterns.push(pattern.split('.').map(str::to_owned).collect());
            }

            by_topic
                .entry(interest.topic.as_ref().to_owned())
                .or_default()
                .push(CompiledInterest {
                    tenant_id: interest.tenant_id,
                    patterns,
                    filter: interest.filter.clone(),
                });
        }

        Ok(Self { by_topic })
    }

    /// How many interests were compiled, across every topic. For assertions
    /// about what a compile produced; matching is the real interface.
    #[must_use]
    pub fn interest_count(&self) -> usize {
        self.by_topic.values().map(Vec::len).sum()
    }
}

impl EventFilter for InterestFilter {
    fn matches(&self, event: &Event) -> bool {
        // Topic equality is the first predicate, and it is also the index: an
        // event on a topic no interest names cannot match, so the lookup failing
        // is the answer rather than a reason to look elsewhere.
        let Some(candidates) = self.by_topic.get(event.topic.as_ref()) else {
            return false;
        };

        candidates.iter().any(|interest| {
            interest.tenant_id == event.tenant_id
                && interest
                    .patterns
                    .iter()
                    .any(|pattern| pattern_matches(pattern, event.r#type.as_ref()))
        })
    }
}

/// Whether a pre-split GTS pattern matches a concrete id.
///
/// Segment-wise, because a wildcard fills its whole segment: `a.*.c` matches
/// `a.b.c` and not `a.bc.d`. A pattern and an id with different segment counts
/// cannot match, which is what stops `a.*` matching `a.b.c`.
fn pattern_matches(pattern: &[String], candidate: &str) -> bool {
    let mut segments = candidate.split('.');
    for expected in pattern {
        match segments.next() {
            Some(actual) if expected == "*" || expected == actual => {}
            _ => return false,
        }
    }
    segments.next().is_none()
}

/// GTS wildcard rules: a wildcard fills its whole dot-separated segment, and at
/// most one segment may be a wildcard.
///
/// Kept here beside the matcher it constrains. The two have to agree - a pattern
/// this accepts must be one `pattern_matches` can evaluate - and keeping them
/// apart is how they drift.
fn validate_type_pattern(pattern: &str) -> Result<(), DomainError> {
    let segments: Vec<&str> = pattern.split('.').collect();
    let wildcard_segments = segments.iter().filter(|s| s.contains('*')).count();
    let partial_wildcard = segments.iter().any(|s| s.contains('*') && *s != "*");

    if wildcard_segments > 1 || partial_wildcard {
        return Err(DomainError::Validation {
            code: "BadTypePattern",
            message: format!(
                "'{pattern}' violates GTS wildcard rules - a wildcard must fill its whole \
                 segment and at most one segment may be a wildcard"
            ),
        });
    }
    Ok(())
}
