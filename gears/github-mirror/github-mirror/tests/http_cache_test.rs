#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use github_mirror::infra::github::cache::{CacheKey, CachedResponse, HttpCache};
use github_mirror::infra::github::compression::Compression;
use github_mirror::infra::storage::sea_orm_repo::SeaOrmHttpCache;
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

const URL: &str = "https://api.github.com/repos/acme/widget/issues";

fn entry() -> CachedResponse {
    CachedResponse {
        body: r#"[{"id":1,"title":"an issue"}]"#.to_owned(),
        etag: Some("W/\"abc\"".to_owned()),
        last_modified: None,
        next_page: Some("https://api.github.com/repos/acme/widget/issues?page=2".to_owned()),
    }
}

async fn store(compression: Compression) -> SeaOrmHttpCache {
    let db = common::inmem_db().await;
    SeaOrmHttpCache::new(Arc::new(DBProvider::<DbError>::new(db)), compression)
}

#[tokio::test]
async fn a_gzipped_entry_round_trips_through_the_database() {
    let cache = store(Compression::Gzip).await;
    let tenant = Uuid::new_v4();
    let key = CacheKey::compute("GET", URL, "application/json");

    assert!(cache.get(tenant, &key).await.unwrap().is_none());

    cache.put(tenant, &key, URL, entry()).await.unwrap();
    let loaded = cache.get(tenant, &key).await.unwrap().expect("entry");
    assert_eq!(loaded, entry(), "compression must be invisible to callers");
}

#[tokio::test]
async fn an_uncompressed_entry_round_trips_too() {
    let cache = store(Compression::None).await;
    let tenant = Uuid::new_v4();
    let key = CacheKey::compute("GET", URL, "application/json");

    cache.put(tenant, &key, URL, entry()).await.unwrap();
    assert_eq!(cache.get(tenant, &key).await.unwrap(), Some(entry()));
}

#[tokio::test]
async fn entries_do_not_cross_tenants() {
    let cache = store(Compression::Gzip).await;
    let key = CacheKey::compute("GET", URL, "application/json");
    let owner = Uuid::new_v4();

    cache.put(owner, &key, URL, entry()).await.unwrap();
    assert!(
        cache.get(Uuid::new_v4(), &key).await.unwrap().is_none(),
        "another tenant must not read this entry"
    );
    assert!(cache.get(owner, &key).await.unwrap().is_some());
}

#[tokio::test]
async fn clearing_by_prefix_drops_only_the_matching_repository() {
    let cache = store(Compression::Gzip).await;
    let tenant = Uuid::new_v4();

    let widget = CacheKey::compute("GET", URL, "application/json");
    let other_url = "https://api.github.com/repos/acme/gadget/issues";
    let gadget = CacheKey::compute("GET", other_url, "application/json");

    cache.put(tenant, &widget, URL, entry()).await.unwrap();
    cache
        .put(tenant, &gadget, other_url, entry())
        .await
        .unwrap();

    let removed = cache
        .clear(tenant, "https://api.github.com/repos/acme/widget")
        .await
        .unwrap();
    assert_eq!(removed, 1);
    assert!(cache.get(tenant, &widget).await.unwrap().is_none());
    assert!(
        cache.get(tenant, &gadget).await.unwrap().is_some(),
        "the other repository's entries must survive"
    );
}
