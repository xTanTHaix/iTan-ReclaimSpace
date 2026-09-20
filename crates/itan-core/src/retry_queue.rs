//! Deferred Retry Queue — handles transient OS-level contention errors during
//! the Atomic Linkage Pipeline.
//!
//! When a hardlink or file-replace operation fails with a sharing/lock violation
//! (Windows `ERROR_SHARING_VIOLATION (32)`, `ERROR_LOCK_VIOLATION (33)`, or POSIX
//! `ETXTBSY`) the failed job is enqueued here for retry with exponential backoff
//! rather than propagating the error to the caller.
//!
//! The retry discipline is:
//! - Maximum 3 attempts per job.
//! - Backoff delay: `100ms × 2^(attempt_number)` (100 ms, 200 ms, 400 ms).
//! - After 3 failed attempts the job is logged and permanently dropped.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use thiserror::Error;

/// Maximum number of retry attempts before a job is permanently dropped.
pub const MAX_RETRY_ATTEMPTS: u8 = 3;

/// Base backoff duration.  Each retry doubles this value.
const BASE_BACKOFF_MS: u64 = 100;

/// Capacity of the bounded channel.  Back-pressure prevents unbounded memory growth
/// under pathological contention (e.g. 500 concurrent sharing violations).
const QUEUE_CAPACITY: usize = 4_096;

/// Errors specific to the retry queue.
#[derive(Debug, Error)]
pub enum RetryQueueError {
    #[error("retry queue is full; job for '{0}' was dropped")]
    QueueFull(PathBuf),
}

// ─── Public types ─────────────────────────────────────────────────────────────

/// A single unit of deferred linkage work.
#[derive(Debug, Clone)]
pub struct RetryJob {
    /// Absolute path to the workspace target file that failed to be replaced.
    pub target: PathBuf,
    /// The slot in the CAS that the target should point to.
    pub slot_path: PathBuf,
    /// Number of attempts already made (0-based; first submission = 0).
    pub attempts: u8,
    /// Monotonic instant after which this job may be retried.
    pub retry_after: Instant,
}

impl RetryJob {
    /// Creates a new `RetryJob` for an initial failure (attempt = 0).
    pub fn new(target: PathBuf, slot_path: PathBuf) -> Self {
        Self {
            retry_after: Instant::now() + backoff_for(0),
            target,
            slot_path,
            attempts: 0,
        }
    }

    /// Returns a new `RetryJob` with the attempt counter incremented and the next
    /// retry deadline computed with exponential backoff.
    ///
    /// Returns `None` when `attempts` has already reached [`MAX_RETRY_ATTEMPTS`],
    /// signalling that the job should be dropped.
    pub fn next_attempt(&self) -> Option<RetryJob> {
        let next = self.attempts + 1;
        if next >= MAX_RETRY_ATTEMPTS {
            return None;
        }
        Some(RetryJob {
            target: self.target.clone(),
            slot_path: self.slot_path.clone(),
            attempts: next,
            retry_after: Instant::now() + backoff_for(next),
        })
    }

    /// Returns `true` if the job's retry deadline has passed and it is ready for rescheduling.
    #[inline]
    pub fn is_ready(&self) -> bool {
        Instant::now() >= self.retry_after
    }
}

/// Computes the exponential backoff delay for a given attempt index.
#[inline]
fn backoff_for(attempt: u8) -> Duration {
    Duration::from_millis(BASE_BACKOFF_MS * (1u64 << attempt))
}

// ─── Queue handle ─────────────────────────────────────────────────────────────

/// A bounded, lock-free channel-based queue for deferred linkage retry jobs.
///
/// Both the sender and receiver sides are cloneable; multiple worker threads can
/// enqueue and drain jobs concurrently without additional locking.
#[derive(Debug, Clone)]
pub struct DeferRetryQueue {
    sender: Sender<RetryJob>,
    receiver: Receiver<RetryJob>,
}

impl DeferRetryQueue {
    /// Creates a new queue with a bounded capacity of [`QUEUE_CAPACITY`] slots.
    pub fn new() -> Self {
        let (sender, receiver) = bounded(QUEUE_CAPACITY);
        Self { sender, receiver }
    }

    /// Enqueues a retry job.
    ///
    /// # Errors
    ///
    /// Returns [`RetryQueueError::QueueFull`] if the queue has reached [`QUEUE_CAPACITY`].
    /// The caller should log and drop the job rather than blocking.
    pub fn enqueue(&self, job: RetryJob) -> Result<(), RetryQueueError> {
        self.sender.try_send(job).map_err(|err| match err {
            TrySendError::Full(j) => RetryQueueError::QueueFull(j.target),
            TrySendError::Disconnected(j) => RetryQueueError::QueueFull(j.target),
        })
    }

    /// Drains all jobs whose retry deadline has passed, returning them as a `Vec`.
    ///
    /// Jobs that are not yet ready are re-enqueued for the next drain cycle.
    pub fn drain_ready(&self) -> Vec<RetryJob> {
        let mut ready = Vec::new();
        let mut requeue = Vec::new();

        // Drain the entire current queue in one pass.
        while let Ok(job) = self.receiver.try_recv() {
            if job.is_ready() {
                ready.push(job);
            } else {
                requeue.push(job);
            }
        }

        // Re-enqueue jobs that are not yet ready.
        for job in requeue {
            // If the queue is now full we drop the job and log — this is a pathological
            // case where drain_ready is called far too frequently.
            if self.sender.try_send(job).is_err() {
                log::warn!("DeferRetryQueue: re-enqueue failed; queue at capacity during drain");
            }
        }

        ready
    }

    /// Returns the current number of items in the queue (approximate under concurrency).
    #[inline]
    pub fn len(&self) -> usize {
        self.receiver.len()
    }

    /// Returns `true` if the queue is currently empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.receiver.is_empty()
    }
}

impl Default for DeferRetryQueue {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn dummy_job() -> RetryJob {
        RetryJob::new(
            PathBuf::from("/workspace/target.dll"),
            PathBuf::from("/store/objects/ab/abc_s000"),
        )
    }

    // P4-U09: a freshly created job at attempt=0 must not be ready immediately.
    #[test]
    fn test_new_job_not_immediately_ready() {
        let job = dummy_job();
        // The retry_after is set 100 ms in the future so it must not be ready yet.
        assert!(!job.is_ready(), "new job must not be immediately ready");
    }

    // Backoff for attempt 0 is 100 ms, attempt 1 is 200 ms, attempt 2 is 400 ms.
    #[test]
    fn test_backoff_exponential() {
        assert_eq!(backoff_for(0), Duration::from_millis(100));
        assert_eq!(backoff_for(1), Duration::from_millis(200));
        assert_eq!(backoff_for(2), Duration::from_millis(400));
    }

    // P4-U10: after MAX_RETRY_ATTEMPTS failures, next_attempt() must return None.
    #[test]
    fn test_retry_job_exhausted_after_max_attempts() {
        let mut job = dummy_job();
        for _ in 0..MAX_RETRY_ATTEMPTS {
            match job.next_attempt() {
                Some(next) => job = next,
                None => return, // Expected termination.
            }
        }
        panic!(
            "Job should have been exhausted within {} attempts",
            MAX_RETRY_ATTEMPTS
        );
    }

    // enqueue and drain_ready round-trip (using a zero-delay job forced via direct construction).
    #[test]
    fn test_enqueue_and_drain_ready() {
        let queue = DeferRetryQueue::new();
        // Create a job with retry_after already in the past.
        let job = RetryJob {
            target: PathBuf::from("/workspace/target.dll"),
            slot_path: PathBuf::from("/store/ab/abc_s000"),
            attempts: 0,
            retry_after: Instant::now() - Duration::from_secs(1), // Already past.
        };
        queue.enqueue(job).expect("enqueue");
        let ready = queue.drain_ready();
        assert_eq!(ready.len(), 1, "one ready job must be drained");
    }

    // A not-yet-ready job must not appear in drain_ready but must remain in the queue.
    #[test]
    fn test_not_ready_job_requeued() {
        let queue = DeferRetryQueue::new();
        let job = dummy_job(); // retry_after = now + 100 ms.
        queue.enqueue(job).expect("enqueue");
        let ready = queue.drain_ready();
        assert!(ready.is_empty(), "future job must not appear in drain");
        assert_eq!(queue.len(), 1, "future job must remain in queue");
    }

    // P4-U: RetryQueueError::QueueFull is returned when the queue is at capacity.
    #[test]
    fn test_queue_full_error() {
        let (sender, receiver) = crossbeam_channel::bounded::<RetryJob>(1);
        let queue = DeferRetryQueue { sender, receiver };
        // Fill the single slot.
        queue.enqueue(dummy_job()).expect("first enqueue");
        // The second enqueue must fail with QueueFull.
        let result = queue.enqueue(dummy_job());
        assert!(
            matches!(result, Err(RetryQueueError::QueueFull(_))),
            "expected QueueFull, got: {:?}",
            result
        );
    }
}
