use super::AlsaError;
use std::time::{Duration, Instant};

// Allow substantially more than the normal 100–500 ms ALSA buffer duration,
// while bounding our retry loops for a stopped device or network audio clock.
// This cannot interrupt an ALSA plugin or kernel ioctl that blocks internally.
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const RETRY_INTERVAL: Duration = Duration::from_millis(10);
const SPIN_BACKOFF: Duration = Duration::from_millis(1);
const MAX_RECOVERIES: usize = 3;

pub(super) trait PlaybackIo {
    /// Returns accepted *frames*, including when the supplied slice contains bytes.
    fn write(&mut self, data: &[u8]) -> Result<usize, alsa::Error>;
    fn wait(&mut self, timeout_ms: u32) -> Result<bool, alsa::Error>;
    fn prepare(&mut self) -> Result<(), alsa::Error>;
    fn resume(&mut self) -> Result<(), alsa::Error>;
    fn drain(&mut self) -> Result<(), alsa::Error>;
    fn discard(&mut self) -> Result<(), alsa::Error>;
}

pub(super) trait Clock {
    fn now(&self) -> Duration;
    fn sleep(&mut self, duration: Duration);
}

pub(super) struct SystemClock(Instant);

impl Default for SystemClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

fn remaining(
    clock: &impl Clock,
    deadline: Duration,
    operation: &'static str,
) -> Result<Duration, AlsaError> {
    let time = deadline.saturating_sub(clock.now());
    if time.is_zero() {
        Err(AlsaError::Timeout(operation))
    } else {
        Ok(time)
    }
}

fn recover(
    io: &mut impl PlaybackIo,
    error: alsa::Error,
    clock: &mut impl Clock,
    deadline: Duration,
    recoveries: &mut usize,
) -> Result<(), AlsaError> {
    if !matches!(error.errno(), libc::EPIPE | libc::ESTRPIPE) {
        return Err(AlsaError::OnWrite(error));
    }
    if *recoveries == MAX_RECOVERIES {
        return Err(AlsaError::RecoveryLimit);
    }
    *recoveries += 1;
    warn!("AlsaSink PCM write failed, recovering and retrying unwritten frames: {error}");

    if error.errno() == libc::ESTRPIPE {
        // snd_pcm_recover can retry resume indefinitely. Keep its resume-first
        // policy, but bound EAGAIN retries and fall back to prepare if unsupported.
        loop {
            let time = remaining(clock, deadline, "resume")?;
            match io.resume() {
                Ok(()) => return Ok(()),
                Err(e) if matches!(e.errno(), libc::EAGAIN | libc::EINTR) => {
                    clock.sleep(RETRY_INTERVAL.min(time));
                }
                Err(_) => break,
            }
        }
    }

    // Preparing after an underrun/suspend can discard frames already queued in
    // ALSA. Do not replay those frames: only the unaccepted suffix is retried.
    remaining(clock, deadline, "recovery")?;
    io.prepare().map_err(AlsaError::OnWrite)
}

fn wait_for_write(
    io: &mut impl PlaybackIo,
    clock: &mut impl Clock,
    deadline: Duration,
    recoveries: &mut usize,
    stalled: &mut bool,
) -> Result<(), AlsaError> {
    let before = clock.now();
    let time = remaining(clock, deadline, "write")?;
    let timeout_ms = POLL_INTERVAL.min(time).as_millis().max(1) as u32;
    let result = io.wait(timeout_ms);

    // Retry immediately after a normal readiness wakeup. Only back off if the
    // previous wakeup also failed to produce any accepted frames. Adding a delay
    // to every wakeup would itself underrun devices with very small buffers.
    let elapsed = clock.now().saturating_sub(before);
    if *stalled && elapsed < SPIN_BACKOFF {
        let time = remaining(clock, deadline, "write")?;
        clock.sleep((SPIN_BACKOFF - elapsed).min(time));
    }
    *stalled = true;

    match result {
        Ok(_) => Ok(()),
        Err(e) if matches!(e.errno(), libc::EAGAIN | libc::EINTR) => Ok(()),
        Err(e) => recover(io, e, clock, deadline, recoveries),
    }
}

fn write_all(
    io: &mut impl PlaybackIo,
    data: &[u8],
    frame_bytes: usize,
    clock: &mut impl Clock,
    deadline: Duration,
) -> Result<(), AlsaError> {
    if frame_bytes == 0 || data.len() % frame_bytes != 0 {
        return Err(AlsaError::InvalidFrameSize);
    }

    let mut offset = 0;
    let mut recoveries = 0;
    let mut stalled = false;
    while offset < data.len() {
        remaining(clock, deadline, "write")?;
        match io.write(&data[offset..]) {
            Ok(0) => wait_for_write(io, clock, deadline, &mut recoveries, &mut stalled)?,
            Ok(frames) => {
                if frames > (data.len() - offset) / frame_bytes {
                    return Err(AlsaError::InvalidWriteSize);
                }
                offset += frames * frame_bytes;
                stalled = false;
            }
            Err(e) if matches!(e.errno(), libc::EAGAIN | libc::EINTR) => {
                wait_for_write(io, clock, deadline, &mut recoveries, &mut stalled)?;
            }
            Err(e) => recover(io, e, clock, deadline, &mut recoveries)?,
        }
    }
    Ok(())
}

fn discard(io: &mut impl PlaybackIo) {
    if let Err(e) = io.discard() {
        debug!("AlsaSink could not discard queued audio before closing: {e}");
    }
}

pub(super) fn write_pending<I: PlaybackIo>(
    pcm: &mut Option<I>,
    buffer: &mut Vec<u8>,
    frame_bytes: usize,
    clock: &mut impl Clock,
) -> Result<(), AlsaError> {
    let deadline = clock.now() + IO_TIMEOUT;
    let result = match pcm.as_mut() {
        Some(io) => write_all(io, buffer, frame_bytes, clock, deadline),
        None => Err(AlsaError::NotConnected),
    };
    buffer.clear();
    if result.is_err() {
        // Release the PCM before returning an error to the player. Its subsequent
        // stop is then harmless, and start can acquire a fresh device handle.
        if let Some(mut io) = pcm.take() {
            discard(&mut io);
        }
    }
    result
}

pub(super) fn finish<I: PlaybackIo>(
    pcm: &mut Option<I>,
    buffer: &mut Vec<u8>,
    period_bytes: usize,
    frame_bytes: usize,
    clock: &mut impl Clock,
) -> Result<(), AlsaError> {
    let Some(mut io) = pcm.take() else {
        buffer.clear();
        return Ok(());
    };
    // Flush and drain share one budget; repeated recovery cannot extend stop.
    let deadline = clock.now() + IO_TIMEOUT;
    let result = (|| {
        if !buffer.is_empty() {
            if buffer.len() > period_bytes {
                return Err(AlsaError::InvalidFrameSize);
            }
            buffer.resize(period_bytes, 0);
            write_all(&mut io, buffer, frame_bytes, clock, deadline)?;
        }
        loop {
            let time = remaining(clock, deadline, "drain")?;
            match io.drain() {
                Ok(()) => return Ok(()),
                Err(e) if matches!(e.errno(), libc::EAGAIN | libc::EINTR) => {
                    // Drain is nonblocking. A short sleep also works when a PCM's
                    // poll descriptor remains ready or reports POLLERR at the end.
                    clock.sleep(RETRY_INTERVAL.min(time));
                }
                Err(e) => return Err(AlsaError::DrainFailure(e)),
            }
        }
    })();
    buffer.clear();
    if result.is_err() {
        discard(&mut io);
    }
    // io is owned here and closes on every path, including a failed tail write.
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    #[derive(Default)]
    struct FakeClock(Duration);

    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            self.0
        }

        fn sleep(&mut self, duration: Duration) {
            self.0 += duration;
        }
    }

    #[derive(Default)]
    struct Calls {
        offered: Vec<Vec<u8>>,
        accepted: Vec<u8>,
        prepared: usize,
        resumed: usize,
        waited: usize,
        drained: usize,
        discarded: usize,
        closed: usize,
    }

    struct FakePcm {
        calls: Rc<RefCell<Calls>>,
        frame_bytes: usize,
        writes: VecDeque<Result<usize, i32>>,
        write_error: Option<i32>,
        prepare_error: Option<i32>,
        resumes: VecDeque<Result<(), i32>>,
        resume_error: Option<i32>,
        drain_error: Option<i32>,
        wait_error: Option<i32>,
    }

    impl FakePcm {
        fn new(frame_bytes: usize, writes: &[Result<usize, i32>]) -> Self {
            Self {
                calls: Rc::default(),
                frame_bytes,
                writes: writes.iter().copied().collect(),
                write_error: None,
                prepare_error: None,
                resumes: VecDeque::new(),
                resume_error: None,
                drain_error: None,
                wait_error: None,
            }
        }
    }

    fn error(errno: i32) -> alsa::Error {
        alsa::Error::new("fake PCM", errno)
    }

    fn result(errno: Option<i32>) -> Result<(), alsa::Error> {
        errno.map_or(Ok(()), |e| Err(error(e)))
    }

    impl PlaybackIo for FakePcm {
        fn write(&mut self, data: &[u8]) -> Result<usize, alsa::Error> {
            let mut calls = self.calls.borrow_mut();
            calls.offered.push(data.to_vec());
            let frames = self
                .writes
                .pop_front()
                .unwrap_or_else(|| self.write_error.map_or(Ok(0), Err))
                .map_err(error)?;
            if frames * self.frame_bytes <= data.len() {
                calls
                    .accepted
                    .extend_from_slice(&data[..frames * self.frame_bytes]);
            }
            Ok(frames)
        }

        fn wait(&mut self, _: u32) -> Result<bool, alsa::Error> {
            self.calls.borrow_mut().waited += 1;
            // Deliberately report immediately ready to exercise spin prevention.
            result(self.wait_error).map(|()| true)
        }

        fn prepare(&mut self) -> Result<(), alsa::Error> {
            self.calls.borrow_mut().prepared += 1;
            result(self.prepare_error)
        }

        fn resume(&mut self) -> Result<(), alsa::Error> {
            self.calls.borrow_mut().resumed += 1;
            self.resumes
                .pop_front()
                .map_or_else(|| result(self.resume_error), |r| r.map_err(error))
        }

        fn drain(&mut self) -> Result<(), alsa::Error> {
            self.calls.borrow_mut().drained += 1;
            result(self.drain_error)
        }

        fn discard(&mut self) -> Result<(), alsa::Error> {
            self.calls.borrow_mut().discarded += 1;
            Ok(())
        }
    }

    impl Drop for FakePcm {
        fn drop(&mut self) {
            self.calls.borrow_mut().closed += 1;
        }
    }

    #[test]
    fn partial_writes_advance_in_frames_for_each_sample_width() {
        for frame_bytes in [4, 6, 8, 16] {
            let io = FakePcm::new(frame_bytes, &[Ok(1), Ok(2), Ok(1)]);
            let calls = Rc::clone(&io.calls);
            let data: Vec<u8> = (0..frame_bytes as u8 * 4).collect();
            let mut buffer = data.clone();
            let mut pcm = Some(io);
            write_pending(
                &mut pcm,
                &mut buffer,
                frame_bytes,
                &mut FakeClock::default(),
            )
            .unwrap();
            let calls = calls.borrow();
            assert_eq!(calls.accepted, data);
            assert_eq!(
                calls.offered,
                [
                    data.clone(),
                    data[frame_bytes..].to_vec(),
                    data[3 * frame_bytes..].to_vec()
                ]
            );
            assert_eq!(calls.prepared, 0);
            assert!(buffer.is_empty());
            assert!(pcm.is_some());
        }
    }

    #[test]
    fn underrun_after_partial_write_retries_only_unaccepted_suffix() {
        let io = FakePcm::new(4, &[Ok(1), Err(libc::EPIPE), Ok(2)]);
        let calls = Rc::clone(&io.calls);
        let data: Vec<u8> = (0..12).collect();
        let mut buffer = data.clone();
        write_pending(&mut Some(io), &mut buffer, 4, &mut FakeClock::default()).unwrap();
        let calls = calls.borrow();
        assert_eq!(calls.accepted, data);
        assert_eq!(
            calls.offered,
            [data.clone(), data[4..].to_vec(), data[4..].to_vec()]
        );
        assert_eq!(calls.prepared, 1);
    }

    #[test]
    fn suspended_write_resumes_and_retries_the_same_frames() {
        let mut io = FakePcm::new(4, &[Err(libc::ESTRPIPE), Ok(2)]);
        io.resumes = [Err(libc::EAGAIN), Ok(())].into();
        let calls = Rc::clone(&io.calls);
        let mut buffer = vec![1; 8];
        write_pending(&mut Some(io), &mut buffer, 4, &mut FakeClock::default()).unwrap();
        let calls = calls.borrow();
        assert_eq!(calls.accepted, vec![1; 8]);
        assert_eq!(calls.resumed, 2);
        assert_eq!(calls.prepared, 0);
    }

    #[test]
    fn unsupported_resume_falls_back_to_prepare() {
        let mut io = FakePcm::new(4, &[Err(libc::ESTRPIPE), Ok(1)]);
        io.resume_error = Some(libc::ENOSYS);
        let calls = Rc::clone(&io.calls);
        write_pending(&mut Some(io), &mut vec![1; 4], 4, &mut FakeClock::default()).unwrap();
        assert_eq!(calls.borrow().prepared, 1);
        assert_eq!(calls.borrow().accepted, vec![1; 4]);
    }

    #[test]
    fn interrupted_or_would_block_write_keeps_unwritten_frames() {
        for errno in [libc::EINTR, libc::EAGAIN] {
            let io = FakePcm::new(4, &[Err(errno), Ok(1)]);
            let calls = Rc::clone(&io.calls);
            let mut clock = FakeClock::default();
            write_pending(&mut Some(io), &mut vec![1; 4], 4, &mut clock).unwrap();
            assert_eq!(calls.borrow().accepted, vec![1; 4]);
            assert_eq!(calls.borrow().prepared, 0);
            assert_eq!(calls.borrow().waited, 1);
            assert_eq!(clock.now(), Duration::ZERO);
        }
    }

    #[test]
    fn poll_underrun_also_recovers_before_retrying() {
        let mut io = FakePcm::new(4, &[Err(libc::EAGAIN), Ok(1)]);
        io.wait_error = Some(libc::EPIPE);
        let calls = Rc::clone(&io.calls);
        write_pending(&mut Some(io), &mut vec![1; 4], 4, &mut FakeClock::default()).unwrap();
        assert_eq!(calls.borrow().prepared, 1);
        assert_eq!(calls.borrow().accepted, vec![1; 4]);
    }

    #[test]
    fn zero_or_would_block_progress_times_out_and_releases_pcm() {
        for errno in [None, Some(libc::EAGAIN)] {
            let mut io = FakePcm::new(4, &[]);
            io.write_error = errno;
            let calls = Rc::clone(&io.calls);
            let mut pcm = Some(io);
            let mut buffer = vec![1; 4];
            let mut clock = FakeClock::default();
            assert!(matches!(
                write_pending(&mut pcm, &mut buffer, 4, &mut clock),
                Err(AlsaError::Timeout("write"))
            ));
            assert!(pcm.is_none());
            assert!(buffer.is_empty());
            assert_eq!(clock.now(), IO_TIMEOUT);
            assert_eq!(calls.borrow().closed, 1);
            assert_eq!(calls.borrow().discarded, 1);
            assert!(calls.borrow().waited <= 2001);
        }
    }

    #[test]
    fn repeated_suspend_is_bounded_and_releases_pcm() {
        let mut io = FakePcm::new(4, &[Err(libc::ESTRPIPE)]);
        io.resume_error = Some(libc::EAGAIN);
        let calls = Rc::clone(&io.calls);
        let mut pcm = Some(io);
        let mut clock = FakeClock::default();
        assert!(matches!(
            write_pending(&mut pcm, &mut vec![1; 4], 4, &mut clock),
            Err(AlsaError::Timeout("resume"))
        ));
        assert!(pcm.is_none());
        assert_eq!(clock.now(), IO_TIMEOUT);
        assert_eq!(calls.borrow().closed, 1);
    }

    #[test]
    fn repeated_underruns_have_a_recovery_limit() {
        let mut io = FakePcm::new(4, &[]);
        io.write_error = Some(libc::EPIPE);
        let calls = Rc::clone(&io.calls);
        let mut pcm = Some(io);
        assert!(matches!(
            write_pending(&mut pcm, &mut vec![1; 4], 4, &mut FakeClock::default()),
            Err(AlsaError::RecoveryLimit)
        ));
        assert!(pcm.is_none());
        assert_eq!(calls.borrow().prepared, MAX_RECOVERIES);
        assert_eq!(calls.borrow().closed, 1);
    }

    #[test]
    fn failed_recovery_discards_pending_audio_and_closes() {
        let mut io = FakePcm::new(4, &[Err(libc::EPIPE)]);
        io.prepare_error = Some(libc::ENODEV);
        let calls = Rc::clone(&io.calls);
        let mut pcm = Some(io);
        let mut buffer = vec![1; 4];
        assert!(write_pending(&mut pcm, &mut buffer, 4, &mut FakeClock::default()).is_err());
        assert!(pcm.is_none());
        assert!(buffer.is_empty());
        assert_eq!(calls.borrow().closed, 1);
    }

    #[test]
    fn finish_pads_only_the_pending_tail_then_closes() {
        let io = FakePcm::new(4, &[Ok(1), Ok(1)]);
        let calls = Rc::clone(&io.calls);
        let mut pcm = Some(io);
        let mut buffer = vec![1; 4];
        finish(&mut pcm, &mut buffer, 8, 4, &mut FakeClock::default()).unwrap();
        assert!(pcm.is_none());
        assert!(buffer.is_empty());
        assert_eq!(calls.borrow().accepted, [1, 1, 1, 1, 0, 0, 0, 0]);
        assert_eq!(calls.borrow().drained, 1);
        assert_eq!(calls.borrow().closed, 1);
    }

    #[test]
    fn finish_does_not_submit_an_empty_extra_period() {
        let io = FakePcm::new(4, &[]);
        let calls = Rc::clone(&io.calls);
        finish(&mut Some(io), &mut vec![], 8, 4, &mut FakeClock::default()).unwrap();
        assert!(calls.borrow().offered.is_empty());
        assert_eq!(calls.borrow().drained, 1);
        assert_eq!(calls.borrow().closed, 1);
    }

    #[test]
    fn stalled_or_failed_drain_releases_pcm() {
        for errno in [libc::EAGAIN, libc::ENODEV] {
            let mut io = FakePcm::new(4, &[]);
            io.drain_error = Some(errno);
            let calls = Rc::clone(&io.calls);
            let mut pcm = Some(io);
            let mut clock = FakeClock::default();
            assert!(finish(&mut pcm, &mut vec![], 8, 4, &mut clock).is_err());
            assert!(pcm.is_none());
            assert!(clock.now() <= IO_TIMEOUT);
            assert_eq!(calls.borrow().closed, 1);
            assert_eq!(calls.borrow().discarded, 1);
        }
    }

    #[test]
    fn failed_tail_write_still_closes_before_stop_returns() {
        let mut io = FakePcm::new(4, &[]);
        io.write_error = Some(libc::ENODEV);
        let calls = Rc::clone(&io.calls);
        let mut pcm = Some(io);
        let mut buffer = vec![1; 4];
        assert!(finish(&mut pcm, &mut buffer, 8, 4, &mut FakeClock::default()).is_err());
        assert!(pcm.is_none());
        assert!(buffer.is_empty());
        assert_eq!(calls.borrow().closed, 1);
        assert_eq!(calls.borrow().drained, 0);
    }
}
