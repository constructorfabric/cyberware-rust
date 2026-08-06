use crate::consumer::{BatchHandlerOutcome, RawEvent};
use crate::error::EventBrokerError;
use crate::sequence::Sequence;

pub(crate) fn processed_count_from_outcome(
    outcome: &BatchHandlerOutcome,
    batch_events: &[RawEvent],
) -> Result<Option<usize>, EventBrokerError> {
    match outcome {
        BatchHandlerOutcome::Success => Ok(Some(batch_events.len())),
        BatchHandlerOutcome::AdvanceThrough { offset } => {
            processed_count_from_delivered_offset(*offset, batch_events).map(Some)
        }
        BatchHandlerOutcome::Retry { .. } => Ok(None),
    }
}

pub(crate) fn processed_count_from_delivered_offset(
    offset: Sequence,
    batch_events: &[RawEvent],
) -> Result<usize, EventBrokerError> {
    batch_events
        .iter()
        .position(|event| event.offset == offset)
        .map(|idx| idx + 1)
        .ok_or_else(|| EventBrokerError::InvalidConsumerOptions {
            detail: format!(
                "offset {offset} is not present in delivered offsets {:?}",
                batch_events
                    .iter()
                    .map(|event| event.offset)
                    .collect::<Vec<_>>()
            ),
            instance: String::new(),
        })
}
