//! Device-independent state used by bounded ALSA recovery.

use crate::{Error, ErrorKind, StreamInstant};
use std::{ops::Range, time::Duration};

pub(super) const MAX_RECOVERY_ATTEMPTS: usize = 100;
pub(super) const RECOVERY_BACKOFF: Duration = Duration::from_millis(10);

#[derive(Debug)]
pub(super) enum WorkerError {
    XRun,
    Suspended,
    Retry,
    Fatal(Error),
}

impl From<alsa::Error> for WorkerError {
    fn from(err: alsa::Error) -> Self {
        match err.errno() {
            libc::EPIPE => Self::XRun,
            // ESTRPIPE is Linux-specific; other hosts classify suspend by PCM state.
            #[cfg(target_os = "linux")]
            libc::ESTRPIPE => Self::Suspended,
            libc::EAGAIN | libc::EINTR => Self::Retry,
            _ => Self::Fatal(err.into()),
        }
    }
}

pub(super) fn fatal(message: impl Into<String>) -> WorkerError {
    WorkerError::Fatal(Error::with_message(
        ErrorKind::StreamInvalidated,
        message.into(),
    ))
}

#[derive(Default)]
pub(super) struct RecoveryBudget(usize);

impl RecoveryBudget {
    pub(super) fn retry(&mut self) -> bool {
        self.0 += 1;
        self.0 <= MAX_RECOVERY_ATTEMPTS
    }

    pub(super) fn progress(&mut self) {
        self.0 = 0;
    }
}

/// Keep a single continuous timeline for callbacks and Stream::now(). A broken
/// hardware clock permanently switches to Instant without jumping backwards.
#[derive(Debug)]
pub(super) struct TimestampClock {
    last: i128,
    fallback_offset: Option<i128>,
}

impl TimestampClock {
    pub(super) fn new(use_hardware: bool) -> Self {
        Self {
            last: 0,
            fallback_offset: (!use_hardware).then_some(0),
        }
    }

    pub(super) fn timestamp(&mut self, hardware: Option<i128>, elapsed: Duration) -> StreamInstant {
        let elapsed = elapsed.as_nanos() as i128;
        let nanos = match (self.fallback_offset, hardware) {
            (None, Some(now)) if now >= self.last => now,
            _ => {
                let offset = *self.fallback_offset.get_or_insert(self.last - elapsed);
                (elapsed + offset).max(self.last)
            }
        };
        self.last = nanos;
        StreamInstant::new(
            (nanos / 1_000_000_000) as u64,
            (nanos % 1_000_000_000) as u32,
        )
    }
}

pub(super) fn timestamp_nanos(ts: libc::timespec) -> Option<i128> {
    (ts.tv_sec >= 0 && (0..1_000_000_000).contains(&ts.tv_nsec))
        .then_some(ts.tv_sec as i128 * 1_000_000_000 + ts.tv_nsec as i128)
}

pub(super) fn hardware_elapsed(now: libc::timespec, origin: libc::timespec) -> Option<i128> {
    let origin = timestamp_nanos(origin).filter(|n| *n != 0)?;
    let now = timestamp_nanos(now).filter(|n| *n != 0)?;
    Some(now - origin)
}

/// Keep an accepted prefix separate from its pending suffix across poll cycles.
#[derive(Default)]
pub(super) struct PendingOutput {
    frames: usize,
    offset: usize,
}

impl PendingOutput {
    pub(super) fn is_empty(&self) -> bool {
        self.offset == self.frames
    }

    pub(super) fn reset(&mut self, frames: usize) {
        self.frames = frames;
        self.offset = 0;
    }

    pub(super) fn write<F>(
        &mut self,
        available: usize,
        frame_size: usize,
        mut write: F,
    ) -> Result<(), WorkerError>
    where
        F: FnMut(Range<usize>) -> Result<usize, alsa::Error>,
    {
        let frames = (self.frames - self.offset).min(available);
        if frames == 0 {
            return Err(WorkerError::Retry);
        }
        let written = write(self.offset * frame_size..(self.offset + frames) * frame_size)?;
        if written == 0 {
            return Err(WorkerError::Retry);
        }
        if written > frames {
            return Err(fatal("ALSA reported more frames written than requested"));
        }
        self.offset += written;
        Ok(())
    }
}

pub(super) fn poll_has_work(flags: alsa::poll::Flags) -> Result<bool, WorkerError> {
    use alsa::poll::Flags;
    if flags.intersects(Flags::HUP | Flags::NVAL) {
        return Err(WorkerError::Fatal(ErrorKind::DeviceNotAvailable.into()));
    }
    Ok(flags.intersects(Flags::IN | Flags::OUT | Flags::ERR))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(sec: i64, nsec: i64) -> libc::timespec {
        libc::timespec {
            tv_sec: sec as _,
            tv_nsec: nsec as _,
        }
    }

    #[test]
    fn stale_htstamp_does_not_depend_on_trigger_time() {
        let mut clock = TimestampClock::new(true);
        let elapsed = hardware_elapsed(ts(233, 344_518_661), ts(233, 300_000_000));
        // This htstamp precedes the reported trigger 233.345061491. It remains
        // valid relative to creation, so supplying audio requires no trigger.
        let first = clock.timestamp(elapsed, Duration::from_millis(45));
        assert_eq!(first, StreamInstant::new(0, 44_518_661));
        assert_eq!(clock.timestamp(elapsed, Duration::from_millis(46)), first);
    }

    #[test]
    fn invalid_or_backwards_hardware_clock_switches_permanently_without_a_jump() {
        for invalid in [None, Some(-1), Some(999_999_999)] {
            let mut clock = TimestampClock::new(true);
            let first = clock.timestamp(Some(1_000_000_000), Duration::from_millis(900));
            assert_eq!(clock.timestamp(invalid, Duration::from_millis(910)), first);
            assert_eq!(
                clock.timestamp(Some(100_000_000_000), Duration::from_millis(920)),
                first + Duration::from_millis(10)
            );
        }
    }

    #[test]
    fn missing_origin_and_invalid_timespec_use_monotonic_time() {
        let mut clock = TimestampClock::new(false);
        assert_eq!(
            clock.timestamp(None, Duration::from_secs(2)),
            StreamInstant::new(2, 0)
        );
        assert_eq!(hardware_elapsed(ts(1, 0), ts(0, 0)), None);
        assert_eq!(hardware_elapsed(ts(0, 0), ts(1, 0)), None);
        assert_eq!(timestamp_nanos(ts(-1, 0)), None);
        assert_eq!(timestamp_nanos(ts(1, 1_000_000_000)), None);
        assert_eq!(timestamp_nanos(ts(1, -1)), None);
    }

    #[test]
    fn timestamps_larger_than_u64_nanoseconds_do_not_wrap() {
        let mut clock = TimestampClock::new(true);
        let seconds = u64::MAX / 1_000_000_000 + 1;
        assert_eq!(
            clock.timestamp(Some(seconds as i128 * 1_000_000_000), Duration::ZERO),
            StreamInstant::new(seconds, 0)
        );
    }

    #[test]
    fn pollerr_reaches_errno_classification_and_disconnect_is_terminal() {
        use alsa::poll::Flags;
        assert!(poll_has_work(Flags::ERR).unwrap());
        assert!(poll_has_work(Flags::ERR | Flags::OUT).unwrap());
        assert!(!poll_has_work(Flags::empty()).unwrap());
        let Err(WorkerError::Fatal(err)) = poll_has_work(Flags::HUP | Flags::ERR) else {
            panic!()
        };
        assert_eq!(err.kind(), ErrorKind::DeviceNotAvailable);
        assert!(matches!(
            WorkerError::from(alsa::Error::new("avail", libc::EPIPE)),
            WorkerError::XRun
        ));
        assert!(matches!(
            WorkerError::from(alsa::Error::new("avail", libc::EIO)),
            WorkerError::Fatal(_)
        ));
    }

    #[test]
    fn repeated_recovery_is_bounded_until_frames_transfer() {
        let mut budget = RecoveryBudget::default();
        for _ in 0..MAX_RECOVERY_ATTEMPTS {
            assert!(budget.retry());
        }
        assert!(!budget.retry());
        budget.progress();
        assert!(budget.retry());
        let WorkerError::Fatal(err) = fatal("budget exhausted") else {
            panic!()
        };
        assert_eq!(err.kind(), ErrorKind::StreamInvalidated);
    }

    #[test]
    fn partial_writes_and_eagain_preserve_the_unwritten_tail() {
        let mut pending = PendingOutput::default();
        pending.reset(5);
        pending
            .write(5, 4, |range| {
                assert_eq!(range, 0..20);
                Ok(2)
            })
            .unwrap();
        assert!(matches!(
            pending.write(5, 4, |range| {
                assert_eq!(range, 8..20);
                Err(alsa::Error::new("writei", libc::EAGAIN))
            }),
            Err(WorkerError::Retry)
        ));
        pending
            .write(2, 4, |range| {
                assert_eq!(range, 8..16);
                Ok(2)
            })
            .unwrap();
        pending
            .write(5, 4, |range| {
                assert_eq!(range, 16..20);
                Ok(1)
            })
            .unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn zero_writes_and_fatal_errors_do_not_retry_inside_write() {
        let mut pending = PendingOutput::default();
        pending.reset(2);
        assert!(matches!(
            pending.write(2, 4, |_| Ok(0)),
            Err(WorkerError::Retry)
        ));
        assert!(matches!(
            pending.write(2, 4, |_| Err(alsa::Error::new("writei", libc::EIO))),
            Err(WorkerError::Fatal(_))
        ));
        assert!(matches!(
            pending.write(2, 4, |_| Ok(3)),
            Err(WorkerError::Fatal(_))
        ));
    }
}
