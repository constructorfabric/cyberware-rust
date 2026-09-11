//! Shared `$filter`/`limit`/`cursor` -> `Page<T>` handling for every list
//! endpoint (`/v1/topics`, `/v1/event-types`, `/v1/consumer-groups`,
//! `/v1/subscriptions`) via `toolkit::api::odata::OData` - the platform's
//! real keyset (`CursorV1`) pagination, not an offset scheme. Each of these
//! endpoints has exactly one supported sort key (its `id`, ascending); none
//! declares an orderby-sortable field (`docs/openapi.yaml`), so a bare
//! `$orderby` with no cursor is accepted but has nothing else to change
//! (`OData` itself already rejects sending both `$orderby` and `cursor`
//! together).
//!
//! Unlike `/v1/events:stream`/`:sse`'s delivery cursor
//! (`domain::model::Cursor`, an inherent sequence/offset position in an
//! append-only log - the one place in this gear where "offset" is the
//! right model), these are generic entity collections with no natural
//! offset semantics: an offset cursor over them silently skips or
//! duplicates rows when the backing map is mutated between page requests.
//! Keyset pagination avoids that by seeking on the resource's own stable
//! `id` instead of a position index.

use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_odata::ast::{CompareOperator, Expr, Value};
use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, Page, PageInfo, SortDir};

const DEFAULT_LIMIT: u64 = 25;
const MAX_LIMIT: u64 = 200;

/// Sorts `items` by `key_of` ascending (this endpoint's one fixed order),
/// seeks to `query.cursor`'s recorded position if present, and returns one
/// keyset page - forward from the cursor, or backward when `cursor.d ==
/// "bwd"` (built by this function itself for `prev_cursor`; a client never
/// constructs one).
///
/// # Errors
/// Returns the mapped `CanonicalError` (`400`) if `query.cursor` doesn't
/// match this endpoint's fixed order or the current `$filter` - a stale or
/// foreign cursor - or if cursor encoding fails.
pub fn paginate_by_key<T: Clone>(
    mut items: Vec<T>,
    query: &ODataQuery,
    key_field: &'static str,
    key_of: impl Fn(&T) -> String,
) -> Result<Page<T>, CanonicalError> {
    items.sort_by_key(|item| key_of(item));
    let total = items.len();

    let order = ODataOrderBy(vec![OrderKey {
        field: key_field.to_owned(),
        dir: SortDir::Asc,
    }]);
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let limit_usize = usize::try_from(limit).unwrap_or(usize::MAX);

    if let Some(cursor) = &query.cursor {
        toolkit_odata::validate_cursor_against(cursor, &order, query.filter_hash.as_deref())?;
    }

    let (start, end) = match &query.cursor {
        None => (0, limit_usize.min(total)),
        Some(cursor) if cursor.d == "bwd" => {
            // `cursor.k[0]` is the first item's key of the page this
            // `prev_cursor` was built from - everything strictly before it,
            // taking the `limit` items closest to it.
            let before = items.partition_point(|item| key_of(item) < cursor.k[0]);
            (before.saturating_sub(limit_usize), before)
        }
        Some(cursor) => {
            // `cursor.k[0]` is the last item's key of the previous page -
            // everything strictly after it.
            let after = items.partition_point(|item| key_of(item) <= cursor.k[0]);
            (after, (after + limit_usize).min(total))
        }
    };

    let page_items = items[start..end].to_vec();

    let next_cursor = match page_items.last() {
        Some(last) if end < total => Some(build_cursor(
            &key_of(last),
            &order,
            query.filter_hash.clone(),
            "fwd",
        )?),
        _ => None,
    };
    let prev_cursor = match page_items.first() {
        Some(first) if start > 0 => Some(build_cursor(
            &key_of(first),
            &order,
            query.filter_hash.clone(),
            "bwd",
        )?),
        _ => None,
    };

    Ok(Page::new(
        page_items,
        PageInfo {
            next_cursor,
            prev_cursor,
            limit,
        },
    ))
}

/// Builds an opaque `CursorV1` token seeking from `key_value` in `direction`
/// (`"fwd"`/`"bwd"`) under `order`/`filter_hash`.
fn build_cursor(
    key_value: &str,
    order: &ODataOrderBy,
    filter_hash: Option<String>,
    direction: &str,
) -> Result<String, CanonicalError> {
    let cursor = CursorV1 {
        k: vec![key_value.to_owned()],
        o: SortDir::Asc,
        s: order.to_signed_tokens(),
        f: filter_hash,
        d: direction.to_owned(),
    };
    cursor
        .encode()
        .map_err(|_| toolkit_odata::Error::CursorInvalidJson.into())
}

/// Evaluates a parsed `$filter` AST against one row, given a way to look up
/// a named field's current value. Supports `eq`/`ne`, `and`/`or`/`not`, and
/// `in` - the subset every list endpoint in this gear actually declares as
/// filterable (bare equality checks per `docs/openapi.yaml`, e.g. `id eq
/// '...'`, `kind eq 'anonymous'`). `Function` nodes and comparison operators
/// other than `eq`/`ne` are not evaluated (the row is excluded) since
/// nothing here declares them as filterable.
///
/// Walks the tree via an explicit heap-allocated work stack rather than
/// native recursion (`And`/`Or`/`Not` are the only recursive variants -
/// `Compare`/`In` operands are always leaf `Identifier`/`Value` nodes), so a
/// maliciously deep `$filter` grows a `Vec` instead of risking a stack
/// overflow. Preserves the original's short-circuit semantics (`And`'s
/// right side is never evaluated if the left is `false`; same for
/// `Or`/`true`). This addresses only the evaluation-time recursion - the
/// `toolkit_odata` parser that produces `Expr` in the first place has its
/// own unbounded recursion, tracked separately.
#[must_use]
pub fn eval_filter(expr: &Expr, get_field: &dyn Fn(&str) -> Option<Value>) -> bool {
    enum Frame<'a> {
        Eval(&'a Expr),
        /// Left side of an `And` evaluated to `true` - the right side still
        /// needs evaluating and its result becomes this node's result.
        AndRight(&'a Expr),
        /// Left side of an `Or` evaluated to `false` - same as `AndRight`.
        OrRight(&'a Expr),
        Negate,
    }

    let mut work = vec![Frame::Eval(expr)];
    let mut results: Vec<bool> = Vec::new();

    while let Some(frame) = work.pop() {
        match frame {
            Frame::Eval(e) => match e {
                Expr::And(l, r) => {
                    work.push(Frame::AndRight(r));
                    work.push(Frame::Eval(l));
                }
                Expr::Or(l, r) => {
                    work.push(Frame::OrRight(r));
                    work.push(Frame::Eval(l));
                }
                Expr::Not(inner) => {
                    work.push(Frame::Negate);
                    work.push(Frame::Eval(inner));
                }
                Expr::Compare(l, op, r) => results.push(eval_compare(l, *op, r, get_field)),
                Expr::In(l, list) => results.push(eval_in(l, list, get_field)),
                Expr::Function(..) | Expr::Identifier(_) | Expr::Value(_) => results.push(false),
            },
            // `results.pop()` can only be `None` here if this function has a
            // bug (every `Eval`/short-circuit branch pushes exactly one
            // result before its combiner frame runs) - `unwrap_or(false)`
            // rather than `expect` both avoids a `panic`-on-malformed-input
            // path and matches this function's own "ambiguous -> exclude
            // the row" philosophy (e.g. the `Function`/`Identifier`/`Value`
            // arm above).
            Frame::AndRight(r) => {
                let left = results.pop().unwrap_or(false);
                if left {
                    work.push(Frame::Eval(r));
                } else {
                    results.push(false); // short-circuit, r never evaluated
                }
            }
            Frame::OrRight(r) => {
                let left = results.pop().unwrap_or(false);
                if left {
                    results.push(true); // short-circuit, r never evaluated
                } else {
                    work.push(Frame::Eval(r));
                }
            }
            Frame::Negate => {
                let inner = results.pop().unwrap_or(false);
                results.push(!inner);
            }
        }
    }

    results.pop().unwrap_or(false)
}

/// `Compare`'s operands are always leaf `Identifier`/`Value` nodes (never
/// compound), so this never recurses - a plain helper, not a stack frame.
fn eval_compare(
    l: &Expr,
    op: CompareOperator,
    r: &Expr,
    get_field: &dyn Fn(&str) -> Option<Value>,
) -> bool {
    let (Expr::Identifier(field), Expr::Value(value)) = (l, r) else {
        return false;
    };
    let Some(actual) = get_field(field) else {
        return false;
    };
    match op {
        CompareOperator::Eq => values_eq(&actual, value),
        CompareOperator::Ne => !values_eq(&actual, value),
        _ => false,
    }
}

/// `In`'s operands are always a leaf `Identifier` plus a list of leaf
/// `Value`s (never compound), so this never recurses either.
fn eval_in(l: &Expr, list: &[Expr], get_field: &dyn Fn(&str) -> Option<Value>) -> bool {
    let Expr::Identifier(field) = l else {
        return false;
    };
    let Some(actual) = get_field(field) else {
        return false;
    };
    list.iter()
        .any(|v| matches!(v, Expr::Value(val) if values_eq(&actual, val)))
}

fn values_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Uuid(x), Value::Uuid(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => x == y,
        (Value::Null, Value::Null) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{eval_filter, paginate_by_key};
    use toolkit_odata::ODataQuery;

    /// Parses `filter` exactly like `ODataQuery::filter()` does, and
    /// evaluates it against a fixed `id`/`kind` row - `and`/`or`/`not`
    /// combinations have zero real coverage anywhere else in this crate
    /// (every handler test uses a single bare `eq`), and this function's
    /// short-circuit stack-frame composition is exactly the part worth
    /// getting direct coverage on.
    fn eval(filter: &str, id: &str, kind: &str) -> bool {
        let expr = toolkit_odata::parse_filter_string(filter)
            .expect("filter must parse")
            .into_expr();
        eval_filter(&expr, &|field| match field {
            "id" => Some(toolkit_odata::ast::Value::String(id.to_owned())),
            "kind" => Some(toolkit_odata::ast::Value::String(kind.to_owned())),
            _ => None,
        })
    }

    #[test]
    fn bare_eq_matches_and_rejects() {
        assert!(eval("id eq 'a'", "a", "anonymous"));
        assert!(!eval("id eq 'a'", "b", "anonymous"));
    }

    #[test]
    fn and_requires_both_sides() {
        assert!(eval("id eq 'a' and kind eq 'anonymous'", "a", "anonymous"));
        assert!(!eval("id eq 'a' and kind eq 'anonymous'", "a", "named"));
        assert!(!eval("id eq 'a' and kind eq 'anonymous'", "b", "anonymous"));
    }

    #[test]
    fn or_requires_either_side() {
        assert!(eval("id eq 'a' or kind eq 'anonymous'", "a", "named"));
        assert!(eval("id eq 'a' or kind eq 'anonymous'", "b", "anonymous"));
        assert!(!eval("id eq 'a' or kind eq 'anonymous'", "b", "named"));
    }

    #[test]
    fn not_negates() {
        assert!(eval("not (id eq 'a')", "b", "anonymous"));
        assert!(!eval("not (id eq 'a')", "a", "anonymous"));
    }

    #[test]
    fn nested_and_or_not_combination() {
        // (id eq 'a' or id eq 'b') and not (kind eq 'named')
        let filter = "(id eq 'a' or id eq 'b') and not (kind eq 'named')";
        assert!(eval(filter, "a", "anonymous"));
        assert!(eval(filter, "b", "anonymous"));
        assert!(!eval(filter, "c", "anonymous"));
        assert!(!eval(filter, "a", "named"));
    }

    #[test]
    fn in_list_matches_any_member() {
        assert!(eval("id in ('a', 'b', 'c')", "b", "anonymous"));
        assert!(!eval("id in ('a', 'b', 'c')", "z", "anonymous"));
    }

    #[test]
    fn deeply_nested_not_chain_does_not_overflow_the_stack() {
        use toolkit_odata::ast::{CompareOperator, Expr, Value};

        // Built directly (not via `parse_filter_string`, which has its own
        // separate, unbounded parse-time recursion - out of scope for this
        // change) to isolate `eval_filter`'s own stack-safety: 100,000
        // nested `Not`s would overflow a native call stack (the whole point
        // of this rewrite) but is trivial for a heap-backed `Vec`.
        let mut expr = Expr::Compare(
            Box::new(Expr::Identifier("id".to_owned())),
            CompareOperator::Eq,
            Box::new(Expr::Value(Value::String("a".to_owned()))),
        );
        for _ in 0..100_000 {
            expr = Expr::Not(Box::new(expr));
        }

        // An even number of negations cancels out.
        assert!(eval_filter(&expr, &|field| match field {
            "id" => Some(Value::String("a".to_owned())),
            _ => None,
        }));

        // `Expr` has no custom `Drop` - the compiler-generated glue for a
        // 100,000-deep `Box<Expr>` chain recursively drops each layer, which
        // would itself overflow the stack (a separate, well-known Rust
        // pitfall for deeply-nested boxed structures, unrelated to
        // `eval_filter`). Leak it deliberately to isolate this test to what
        // it's actually verifying.
        std::mem::forget(expr);
    }

    #[test]
    fn keyset_returns_everything_in_one_page_when_it_all_fits() {
        let items: Vec<String> = ('a'..='e').map(String::from).collect();

        let page1 = paginate_by_key(items, &ODataQuery::new(), "id", Clone::clone)
            .expect("page 1 must paginate");
        assert_eq!(page1.items, vec!["a", "b", "c", "d", "e"]);
        assert!(page1.page_info.next_cursor.is_none());
        assert!(page1.page_info.prev_cursor.is_none());
    }

    #[test]
    fn keyset_pages_forward_across_the_full_set() {
        let items: Vec<String> = ('a'..='e').map(String::from).collect();
        let mut query = ODataQuery::new().with_limit(2);

        let page1 = paginate_by_key(items.clone(), &query, "id", Clone::clone)
            .expect("page 1 must paginate");
        assert_eq!(page1.items, vec!["a", "b"]);
        assert!(page1.page_info.prev_cursor.is_none());
        let next = page1.page_info.next_cursor.expect("more items remain");

        query.cursor = Some(toolkit_odata::CursorV1::decode(&next).expect("cursor decodes"));
        let page2 = paginate_by_key(items.clone(), &query, "id", Clone::clone)
            .expect("page 2 must paginate");
        assert_eq!(page2.items, vec!["c", "d"]);
        let prev = page2
            .page_info
            .prev_cursor
            .clone()
            .expect("page 2 has a predecessor");
        let next2 = page2.page_info.next_cursor.expect("more items remain");

        query.cursor = Some(toolkit_odata::CursorV1::decode(&next2).expect("cursor decodes"));
        let page3 = paginate_by_key(items.clone(), &query, "id", Clone::clone)
            .expect("page 3 must paginate");
        assert_eq!(page3.items, vec!["e"]);
        assert!(page3.page_info.next_cursor.is_none());

        // Walking `prev_cursor` from page 2 must return exactly page 1.
        query.cursor = Some(toolkit_odata::CursorV1::decode(&prev).expect("cursor decodes"));
        let back_to_page1 = paginate_by_key(items, &query, "id", Clone::clone)
            .expect("backward page must paginate");
        assert_eq!(back_to_page1.items, vec!["a", "b"]);
        assert!(back_to_page1.page_info.prev_cursor.is_none());
    }

    #[test]
    fn keyset_rejects_a_cursor_from_a_different_filter() {
        let items: Vec<String> = ('a'..='c').map(String::from).collect();
        let filtered = ODataQuery::new()
            .with_limit(1)
            .with_filter_hash("hash-a".to_owned());
        let page1 =
            paginate_by_key(items.clone(), &filtered, "id", Clone::clone).expect("must paginate");
        let next = page1.page_info.next_cursor.expect("more items remain");

        let mut different_filter = ODataQuery::new()
            .with_limit(1)
            .with_filter_hash("hash-b".to_owned());
        different_filter.cursor = Some(toolkit_odata::CursorV1::decode(&next).expect("decodes"));

        paginate_by_key(items, &different_filter, "id", Clone::clone)
            .expect_err("cursor from a different filter must be rejected");
    }
}
