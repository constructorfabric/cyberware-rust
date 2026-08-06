//! Proc-macro implementation for `#[temporary]`.
//!
//! Marks an item (struct, enum, impl block, fn, ...) as a temporary
//! stand-in slated for replacement by tracked follow-up work. No-op at
//! runtime - expands to the item unchanged plus an injected `#[doc]` line,
//! so it's both greppable (`grep -rn "TEMPORARY("`) and visible in
//! rustdoc/IDE hover.

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{Meta, Token};

use crate::utils::parse_string_attribute;

pub fn expand_temporary(attr: TokenStream, item: &TokenStream) -> syn::Result<TokenStream> {
    let metas = Punctuated::<Meta, Token![,]>::parse_terminated.parse2(attr)?;

    let mut tracking: Option<String> = None;
    let mut reason: Option<String> = None;
    for meta in &metas {
        if let Some(value) = parse_string_attribute("tracking", meta)? {
            tracking = Some(value);
            continue;
        }
        if let Some(value) = parse_string_attribute("reason", meta)? {
            reason = Some(value);
            continue;
        }
        return Err(syn::Error::new_spanned(
            meta,
            "unknown #[temporary] argument - expected `tracking` or `reason`",
        ));
    }

    let tracking = tracking.ok_or_else(|| {
        syn::Error::new(
            Span::call_site(),
            "#[temporary] requires tracking = \"<repo>#<issue>\", e.g. \
             tracking = \"gears-rust#4347\"",
        )
    })?;
    let reason = reason.ok_or_else(|| {
        syn::Error::new(
            Span::call_site(),
            "#[temporary] requires reason = \"<why this is temporary>\"",
        )
    })?;

    validate_tracking_ref(&tracking)?;
    if reason.trim().is_empty() {
        return Err(syn::Error::new(
            Span::call_site(),
            "#[temporary] reason must not be empty",
        ));
    }

    let doc_line = format!("**TEMPORARY** (tracked by `{tracking}`): {reason}");
    Ok(quote! {
        #[doc = ""]
        #[doc = #doc_line]
        #item
    })
}

/// `<repo>#<issue>`, e.g. `gears-rust#4347` or `cargo-gears#89` - loosely
/// validated (ASCII kebab/underscore repo name, decimal issue number) so a
/// typo like a missing `#` doesn't silently produce an unreferenced marker.
fn validate_tracking_ref(tracking: &str) -> syn::Result<()> {
    let err = || {
        syn::Error::new(
            Span::call_site(),
            format!(
                "#[temporary] tracking = \"{tracking}\" must look like \
                 \"<repo>#<issue-number>\", e.g. \"gears-rust#4347\""
            ),
        )
    };
    let Some((repo, issue)) = tracking.split_once('#') else {
        return Err(err());
    };
    let repo_ok = !repo.is_empty()
        && repo
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let issue_ok = !issue.is_empty() && issue.chars().all(|c| c.is_ascii_digit());
    if repo_ok && issue_ok {
        Ok(())
    } else {
        Err(err())
    }
}
