//! Unit tests for offset-store built-in implementations.
use event_broker_sdk::Sequence;
use event_broker_sdk::{
    CommitOffset, ConsumerGroupId, Fallback, InMemoryOffsetManager, OffsetStore, Position, TopicId,
};

fn group() -> ConsumerGroupId {
    ConsumerGroupId::new(uuid::Uuid::new_v4())
}

fn topic() -> TopicId {
    TopicId::from_gts("gts.cf.core.events.topic.v1~example.orders.v1")
}

#[tokio::test]
async fn in_memory_position_fresh_returns_fallback() {
    let om = InMemoryOffsetManager::new(Fallback::Earliest);
    let g = group();
    let t = topic();
    assert_eq!(
        om.load_position(&g, &t, 0).await.unwrap(),
        Position::Earliest
    );
}

#[tokio::test]
async fn in_memory_position_returns_stored_value_verbatim() {
    let om = InMemoryOffsetManager::new(Fallback::Latest);
    let g = group();
    let t = topic();
    om.commit(&g, &t, 0, Sequence::assigned(42)).await.unwrap();
    // Exact carries the last-processed offset verbatim (no +1 on the SDK side).
    assert_eq!(
        om.load_position(&g, &t, 0).await.unwrap(),
        Position::Exact(Sequence::assigned(42))
    );
}

#[tokio::test]
async fn in_memory_commit_applies_max_semantics() {
    let om = InMemoryOffsetManager::new(Fallback::Earliest);
    let g = group();
    let t = topic();
    om.commit(&g, &t, 1, Sequence::assigned(100)).await.unwrap();
    om.commit(&g, &t, 1, Sequence::assigned(50)).await.unwrap();
    assert_eq!(
        om.load_position(&g, &t, 1).await.unwrap(),
        Position::Exact(Sequence::assigned(100))
    );
    om.commit(&g, &t, 1, Sequence::assigned(200)).await.unwrap();
    assert_eq!(
        om.load_position(&g, &t, 1).await.unwrap(),
        Position::Exact(Sequence::assigned(200))
    );
}

#[tokio::test]
async fn in_memory_overrides_apply_only_when_no_committed_cursor() {
    let om = InMemoryOffsetManager::new(Fallback::Latest)
        .with_overrides([((topic(), 0), Sequence::assigned(17))]);
    let g = group();
    let t = topic();

    // Override applies when no committed cursor.
    assert_eq!(
        om.load_position(&g, &t, 0).await.unwrap(),
        Position::Exact(Sequence::assigned(17))
    );

    // Committed cursor wins over override.
    om.commit(&g, &t, 0, Sequence::assigned(100)).await.unwrap();
    assert_eq!(
        om.load_position(&g, &t, 0).await.unwrap(),
        Position::Exact(Sequence::assigned(100))
    );
}

#[tokio::test]
async fn in_memory_override_miss_falls_back_to_fallback() {
    let om = InMemoryOffsetManager::new(Fallback::Latest)
        .with_overrides([((topic(), 0), Sequence::assigned(17))]);
    let g = group();
    let t = topic();
    // Partition not in overrides → falls back to Fallback::Latest.
    assert_eq!(om.load_position(&g, &t, 9).await.unwrap(), Position::Latest);
}
