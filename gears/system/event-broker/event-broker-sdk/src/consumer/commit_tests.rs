#[cfg(feature = "db")]
mod tx {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use uuid::Uuid;

    use super::super::commit::TxCommitHandleParts;
    use crate::consumer::{
        CommitOffsetInTx, Fallback, OffsetManagerError, OffsetStore, Position, TxCommitHandle,
    };
    use crate::ids::{ConsumerGroupId, TopicId};
    use crate::sequence::Sequence;

    type RecordedCommit = (ConsumerGroupId, TopicId, u32, i64);
    type RecordedCommits = Mutex<Vec<RecordedCommit>>;

    #[derive(Default)]
    struct RecordingTxOffsetManager {
        calls: RecordedCommits,
    }

    #[async_trait]
    impl OffsetStore for RecordingTxOffsetManager {
        async fn load_position(
            &self,
            _group: &ConsumerGroupId,
            _topic: &TopicId,
            _partition: u32,
        ) -> Result<Position, OffsetManagerError> {
            Ok(Fallback::Earliest.into())
        }
    }

    #[async_trait]
    impl CommitOffsetInTx for RecordingTxOffsetManager {
        async fn commit_in_tx<TX>(
            &self,
            _txn: &TX,
            group: &ConsumerGroupId,
            topic: &TopicId,
            partition: u32,
            offset: Sequence,
        ) -> Result<(), OffsetManagerError>
        where
            TX: toolkit_db::secure::DBRunner + Sync,
        {
            self.calls.lock().expect("recording mutex").push((
                *group,
                *topic,
                partition,
                offset.as_i64(),
            ));
            Ok(())
        }
    }

    #[tokio::test]
    async fn tx_commit_handle_persists_explicit_offset_and_records_commit_state() {
        let db = toolkit_db::connect_db(
            "sqlite::memory:",
            toolkit_db::ConnectOpts {
                max_conns: Some(1),
                ..toolkit_db::ConnectOpts::default()
            },
        )
        .await
        .expect("sqlite db");

        let manager = Arc::new(RecordingTxOffsetManager::default());
        let group = ConsumerGroupId::new(Uuid::new_v4());
        let topic = TopicId::new(Uuid::new_v4());
        let handle = TxCommitHandle::new(TxCommitHandleParts {
            partition: 5,
            offsets: vec![Sequence::assigned(123)],
            offset_manager: manager.clone(),
            group,
            topic,
        });
        let committed = handle.committed_offset.clone();

        assert_eq!(*committed.lock().expect("commit state mutex"), None);
        db.transaction_ref(move |tx| {
            Box::pin(async move {
                handle
                    .commit_offset_in_tx(tx, Sequence::assigned(123))
                    .await
                    .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
                Ok(())
            })
        })
        .await
        .expect("transaction");

        assert_eq!(
            *committed.lock().expect("commit state mutex"),
            Some(Sequence::assigned(123))
        );
        assert_eq!(
            manager.calls.lock().expect("recording mutex").as_slice(),
            &[(group, topic, 5, 123)]
        );
    }

    #[tokio::test]
    async fn tx_commit_handle_commits_batch_success_offset_in_transaction() {
        let db = toolkit_db::connect_db(
            "sqlite::memory:",
            toolkit_db::ConnectOpts {
                max_conns: Some(1),
                ..toolkit_db::ConnectOpts::default()
            },
        )
        .await
        .expect("sqlite db");

        let manager = Arc::new(RecordingTxOffsetManager::default());
        let group = ConsumerGroupId::new(Uuid::new_v4());
        let topic = TopicId::new(Uuid::new_v4());
        let handle = TxCommitHandle::new(TxCommitHandleParts {
            partition: 5,
            offsets: vec![
                Sequence::assigned(10),
                Sequence::assigned(11),
                Sequence::assigned(12),
            ],
            offset_manager: manager.clone(),
            group,
            topic,
        });
        let committed = handle.committed_offset.clone();

        db.transaction_ref(move |tx| {
            Box::pin(async move {
                handle
                    .commit_offset_in_tx(tx, Sequence::assigned(12))
                    .await
                    .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
                Ok(())
            })
        })
        .await
        .expect("transaction");

        assert_eq!(
            manager.calls.lock().expect("recording mutex").as_slice(),
            &[(group, topic, 5, 12)]
        );
        assert_eq!(
            *committed.lock().expect("commit state mutex"),
            Some(Sequence::assigned(12))
        );
    }

    #[tokio::test]
    async fn tx_commit_handle_commits_delivered_prefix_offset_in_transaction() {
        let db = toolkit_db::connect_db(
            "sqlite::memory:",
            toolkit_db::ConnectOpts {
                max_conns: Some(1),
                ..toolkit_db::ConnectOpts::default()
            },
        )
        .await
        .expect("sqlite db");

        let manager = Arc::new(RecordingTxOffsetManager::default());
        let group = ConsumerGroupId::new(Uuid::new_v4());
        let topic = TopicId::new(Uuid::new_v4());
        let handle = TxCommitHandle::new(TxCommitHandleParts {
            partition: 5,
            offsets: vec![
                Sequence::assigned(10),
                Sequence::assigned(17),
                Sequence::assigned(19),
            ],
            offset_manager: manager.clone(),
            group,
            topic,
        });
        let committed = handle.committed_offset.clone();

        db.transaction_ref(move |tx| {
            Box::pin(async move {
                handle
                    .commit_offset_in_tx(tx, Sequence::assigned(17))
                    .await
                    .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
                Ok(())
            })
        })
        .await
        .expect("transaction");

        assert_eq!(
            manager.calls.lock().expect("recording mutex").as_slice(),
            &[(group, topic, 5, 17)]
        );
        assert_eq!(
            *committed.lock().expect("commit state mutex"),
            Some(Sequence::assigned(17))
        );
    }

    #[tokio::test]
    async fn tx_commit_handle_rejects_offset_outside_delivered_batch_before_write() {
        let db = toolkit_db::connect_db(
            "sqlite::memory:",
            toolkit_db::ConnectOpts {
                max_conns: Some(1),
                ..toolkit_db::ConnectOpts::default()
            },
        )
        .await
        .expect("sqlite db");

        let manager = Arc::new(RecordingTxOffsetManager::default());
        let group = ConsumerGroupId::new(Uuid::new_v4());
        let topic = TopicId::new(Uuid::new_v4());
        let handle = TxCommitHandle::new(TxCommitHandleParts {
            partition: 5,
            offsets: vec![
                Sequence::assigned(10),
                Sequence::assigned(11),
                Sequence::assigned(12),
            ],
            offset_manager: manager.clone(),
            group,
            topic,
        });
        let committed = handle.committed_offset.clone();

        let err = db
            .transaction_ref(move |tx| {
                Box::pin(async move {
                    handle
                        .commit_offset_in_tx(tx, Sequence::assigned(20))
                        .await
                        .map_err(|err| toolkit_db::DbError::InvalidConfig(err.to_string()))?;
                    Ok(())
                })
            })
            .await
            .expect_err("outside offset must be rejected");

        assert!(
            err.to_string()
                .contains("not present in delivered batch offsets")
        );
        assert!(manager.calls.lock().expect("recording mutex").is_empty());
        assert_eq!(*committed.lock().expect("commit state mutex"), None);
    }
}
