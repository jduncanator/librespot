use std::process::exit;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait};
use thiserror::Error;

use super::{Sink, SinkError, SinkResult};
use crate::config::AudioFormat;
use crate::convert::Converter;
use crate::decoder::AudioPacket;
use crate::{NUM_CHANNELS, SAMPLE_RATE};

const MAX_QUEUED_SOURCES: usize = 26;
const QUEUE_POLL_INTERVAL: Duration = Duration::from_millis(10);
const QUEUE_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const RECOVERY_BACKOFF: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_millis(250),
    Duration::from_millis(500),
];
const RECOVERY_RESET_AFTER: Duration = Duration::from_secs(30);

#[cfg(all(
    feature = "rodiojack-backend",
    not(any(target_os = "linux", target_os = "dragonfly", target_os = "freebsd"))
))]
compile_error!("Rodio JACK backend is currently only supported on linux.");

#[cfg(feature = "rodio-backend")]
pub fn mk_rodio(device: Option<String>, format: AudioFormat) -> Box<dyn Sink> {
    Box::new(open(cpal::default_host(), device, format))
}

#[cfg(feature = "rodiojack-backend")]
pub fn mk_rodiojack(device: Option<String>, format: AudioFormat) -> Box<dyn Sink> {
    Box::new(open(
        cpal::host_from_id(cpal::HostId::Jack).unwrap(),
        device,
        format,
    ))
}

#[derive(Debug, Error)]
pub enum RodioError {
    #[error("<RodioSink> No Device Available")]
    NoDeviceAvailable,
    #[error("<RodioSink> device \"{0}\" is Not Available")]
    DeviceNotAvailable(String),
    #[error("<RodioSink> device name \"{0}\" is ambiguous; use a device ID from --device '?'")]
    AmbiguousDeviceName(String),
    #[error("<RodioSink> Play Error: {0}")]
    PlayError(#[from] rodio::PlayError),
    #[error("<RodioSink> Stream Error: {0}")]
    StreamError(#[from] rodio::DeviceSinkError),
    #[error("<RodioSink> Cannot Get Audio Devices: {0}")]
    DevicesError(#[from] cpal::Error),
    #[error("<RodioSink> {0}")]
    Samples(String),
    #[error("<RodioSink> Output stream failed: {0}")]
    OutputFailed(String),
    #[error("<RodioSink> Timed out waiting for the output queue")]
    OutputStalled,
    #[error("<RodioSink> Output recovery exhausted: {0}")]
    RecoveryExhausted(String),
}

impl From<RodioError> for SinkError {
    fn from(e: RodioError) -> SinkError {
        use RodioError::*;
        let es = e.to_string();
        match e {
            StreamError(_) | PlayError(_) | Samples(_) | OutputFailed(_) | OutputStalled
            | RecoveryExhausted(_) => SinkError::OnWrite(es),
            NoDeviceAvailable | DeviceNotAvailable(_) => SinkError::ConnectionRefused(es),
            DevicesError(_) | AmbiguousDeviceName(_) => SinkError::InvalidParams(es),
        }
    }
}

pub struct RodioSink {
    output: Option<RodioOutput>,
    output_config: OutputConfig,
    recovery: RecoveryPolicy,
}

struct RodioOutput {
    sink: rodio::Player,
    _stream: rodio::MixerDeviceSink,
    errors: Receiver<cpal::Error>,
    observed_queue_len: usize,
}

impl RodioOutput {
    fn new(mut stream: rodio::MixerDeviceSink, errors: Receiver<cpal::Error>) -> Self {
        stream.log_on_drop(false);
        Self {
            sink: rodio::Player::connect_new(stream.mixer()),
            _stream: stream,
            errors,
            observed_queue_len: 0,
        }
    }

    fn error(&self) -> Option<RodioError> {
        match self.errors.try_recv() {
            Ok(error) => Some(RodioError::OutputFailed(error.to_string())),
            // A backend may not retain an error callback if it cannot report
            // asynchronous errors. Queue deadlines also detect silent stalls.
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    fn queue_progress(&mut self) -> (usize, bool) {
        let queued = self.sink.len();
        let progressed = queued < self.observed_queue_len;
        self.observed_queue_len = queued;
        (queued, progressed)
    }
}

// Reopen the selected device ID with the configuration that actually succeeded,
// including any initial format/rate fallback. Do not fall back to a different
// ID or renegotiate the network audio clock while recovering. Host aliases such
// as ALSA's "default" retain their host-defined routing behavior.
struct OutputConfig {
    host: cpal::Host,
    device_id: cpal::DeviceId,
    config: cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
}

impl OutputConfig {
    fn open(&self) -> Result<RodioOutput, RodioError> {
        // The default device may be absent from enumeration. Match its ID,
        // never its current display name, before searching the remaining IDs.
        let device = self
            .host
            .default_output_device()
            .filter(|device| device.id().ok().as_ref() == Some(&self.device_id))
            .or_else(|| self.host.device_by_id(&self.device_id))
            .ok_or_else(|| RodioError::DeviceNotAvailable(self.device_id.to_string()))?;
        let (errors, callback) = stream_error_channel();
        let stream = rodio::DeviceSinkBuilder::default()
            .with_device(device)
            .with_config(&self.config)
            .with_sample_format(self.sample_format)
            .with_error_callback(callback)
            .open_stream()?;
        let output = RodioOutput::new(stream, errors);
        if let Some(error) = output.error() {
            return Err(error);
        }
        Ok(output)
    }
}

fn stream_error_channel() -> (
    Receiver<cpal::Error>,
    impl FnMut(cpal::Error) + Send + Clone + 'static,
) {
    let (sender, receiver) = mpsc::sync_channel(1);
    (receiver, move |error: cpal::Error| {
        // The callback can run on CPAL's audio thread. Never log, wait, or drop
        // the MixerDeviceSink here (dropping it can join that same thread).
        // CPAL also reports recoverable notifications through this callback.
        // They must not occupy the single terminal-error slot or trigger a
        // close/reopen cycle while the existing stream is still usable.
        if matches!(
            error.kind(),
            cpal::ErrorKind::Xrun
                | cpal::ErrorKind::RealtimeDenied
                | cpal::ErrorKind::DeviceChanged
        ) {
            return;
        }
        // Keep only the first terminal error for this output generation.
        let _ = sender.try_send(error);
    })
}

#[derive(Default)]
struct RecoveryPolicy {
    attempts: usize,
    last_attempt: Option<Instant>,
}

impl RecoveryPolicy {
    fn record_progress(&mut self, now: Instant) {
        if self
            .last_attempt
            .is_some_and(|last| now.duration_since(last) >= RECOVERY_RESET_AFTER)
        {
            *self = Self::default();
        }
    }

    fn retry<T>(
        &mut self,
        mut cause: RodioError,
        mut now: impl FnMut() -> Instant,
        mut wait: impl FnMut(Duration),
        mut open: impl FnMut() -> Result<T, RodioError>,
    ) -> Result<T, RodioError> {
        while let Some(&delay) = RECOVERY_BACKOFF.get(self.attempts) {
            self.attempts += 1;
            self.last_attempt = Some(now());
            wait(delay);
            match open() {
                Ok(output) => return Ok(output),
                Err(error) => {
                    warn!("Unable to reopen Rodio output: {error}");
                    cause = error;
                }
            }
        }
        Err(RodioError::RecoveryExhausted(cause.to_string()))
    }
}

struct QueueWait {
    started: Instant,
}

impl QueueWait {
    fn poll(
        &self,
        now: Instant,
        queued: usize,
        limit: usize,
        error: Option<RodioError>,
    ) -> Result<bool, RodioError> {
        if let Some(error) = error {
            Err(error)
        } else if queued <= limit {
            Ok(true)
        } else if now.duration_since(self.started) >= QUEUE_WAIT_TIMEOUT {
            Err(RodioError::OutputStalled)
        } else {
            Ok(false)
        }
    }
}

fn device_name(device: &cpal::Device) -> Result<String, cpal::Error> {
    device
        .description()
        .map(|description| description.name().to_owned())
}

fn matches_device_id(device: &cpal::Device, requested: &str) -> bool {
    device
        .id()
        .is_ok_and(|id| id.to_string() == requested || id.id() == requested)
}

fn select_device(host: &cpal::Host, requested: &str) -> Result<cpal::Device, RodioError> {
    // The default can be usable without appearing in enumeration, particularly
    // with a minimal ALSA configuration. Preserve --device default as well as
    // the new fully qualified CPAL IDs.
    let default = host.default_output_device();
    if let Some(device) = default.as_ref().filter(|d| matches_device_id(d, requested)) {
        return Ok(device.clone());
    }

    let mut devices: Vec<_> = default.into_iter().collect();
    for device in host.output_devices()? {
        if !devices.contains(&device) {
            devices.push(device);
        }
    }
    // IDs take precedence over human-readable descriptions. ALSA's hw/plughw
    // variants can share a description while opening different audio routes.
    if let Some(device) = devices.iter().find(|d| matches_device_id(d, requested)) {
        return Ok(device.clone());
    }

    let mut matches = devices.into_iter().filter(|device| {
        device_name(device)
            .ok()
            .is_some_and(|name| name == requested)
    });
    let device = matches
        .next()
        .ok_or_else(|| RodioError::DeviceNotAvailable(requested.to_owned()))?;
    if matches.next().is_some() {
        return Err(RodioError::AmbiguousDeviceName(requested.to_owned()));
    }
    Ok(device)
}

fn list_device(device: &cpal::Device) {
    let name = device_name(device).unwrap_or_else(|_| "[unknown name]".to_owned());
    match device.id() {
        Ok(id) => println!("  {id} ({name})"),
        Err(e) => {
            warn!("Cannot get device ID: {e}");
            println!("  {name}");
        }
    }
    list_formats(device);
}

fn list_formats(device: &cpal::Device) {
    match device.default_output_config() {
        Ok(cfg) => {
            debug!("  Default config:");
            debug!("    {cfg:?}");
        }
        Err(e) => {
            // Use loglevel debug, since even the output is only debug
            debug!("Error getting default Rodio output config: {e}");
        }
    };

    match device.supported_output_configs() {
        Ok(mut cfgs) => {
            if let Some(first) = cfgs.next() {
                debug!("  Available configs:");
                debug!("    {first:?}");
            } else {
                return;
            }

            for cfg in cfgs {
                debug!("    {cfg:?}");
            }
        }
        Err(e) => {
            debug!("Error getting supported Rodio output configs: {e}");
        }
    }
}

fn list_outputs(host: &cpal::Host) -> Result<(), cpal::Error> {
    let default = host.default_output_device();
    let default_id = default.as_ref().and_then(|device| device.id().ok());
    if let Some(default_device) = &default {
        println!("Default Audio Device:");
        list_device(default_device);
        println!("Other Available Audio Devices:");
    } else {
        warn!("No default device was found");
    }

    for device in host.output_devices()? {
        if default_id
            .as_ref()
            .is_some_and(|id| device.id().ok().as_ref() == Some(id))
        {
            continue;
        }
        list_device(&device);
    }

    Ok(())
}

fn create_sink(
    host: cpal::Host,
    device: Option<String>,
    format: AudioFormat,
) -> Result<(RodioOutput, OutputConfig), RodioError> {
    let cpal_device = match device.as_deref() {
        Some("?") => match list_outputs(&host) {
            Ok(()) => exit(0),
            Err(e) => {
                error!("{e}");
                exit(1);
            }
        },
        Some(requested) => select_device(&host, requested)?,
        None => host
            .default_output_device()
            .ok_or(RodioError::NoDeviceAvailable)?,
    };

    let name = device_name(&cpal_device).ok();
    let id = cpal_device.id()?;
    info!(
        "Using audio device: {} ({})",
        id,
        name.as_deref().unwrap_or("[unknown name]")
    );

    // First try native stereo 44.1 kHz playback, then fall back to the device default sample rate
    // (some devices only support 48 kHz and Rodio will resample linearly), then fall back to
    // whatever the default device config is (like mono).
    let default_config = cpal_device
        .default_output_config()
        .map_err(|_| RodioError::NoDeviceAvailable)?;
    let config = cpal_device
        .supported_output_configs()
        .map_err(|_| RodioError::NoDeviceAvailable)?
        .find(|c| c.channels() == NUM_CHANNELS as cpal::ChannelCount)
        .and_then(|c| {
            c.try_with_sample_rate(SAMPLE_RATE)
                .or_else(|| c.try_with_sample_rate(default_config.sample_rate()))
        })
        .unwrap_or(default_config);

    let sample_format = match format {
        AudioFormat::F64 => cpal::SampleFormat::F64,
        AudioFormat::F32 => cpal::SampleFormat::F32,
        AudioFormat::S32 => cpal::SampleFormat::I32,
        AudioFormat::S24 | AudioFormat::S24_3 => cpal::SampleFormat::I24,
        AudioFormat::S16 => cpal::SampleFormat::I16,
    };

    // Keep Rodio's safe buffer-size request from from_device(). Using
    // with_config() here would replace it with CPAL's BufferSize::Default.
    let (mut errors, callback) = stream_error_channel();
    let builder = rodio::DeviceSinkBuilder::from_device(cpal_device.clone())?
        .with_channels(rodio::ChannelCount::new(config.channels()).expect("nonzero CPAL channels"))
        .with_sample_rate(rodio::SampleRate::new(config.sample_rate()).expect("nonzero CPAL rate"))
        .with_sample_format(sample_format)
        .with_error_callback(callback);
    #[cfg(feature = "rodiojack-backend")]
    let builder = if host.id() == cpal::HostId::Jack {
        // JACK owns its period size; an unrelated fixed request is rejected.
        builder.with_buffer_size(cpal::BufferSize::Default)
    } else {
        builder
    };
    let stream = match builder.open_stream() {
        Ok(exact_stream) => exact_stream,
        Err(e) => {
            warn!("unable to create Rodio output, falling back to default: {e}");
            let (fallback_errors, callback) = stream_error_channel();
            errors = fallback_errors;
            rodio::DeviceSinkBuilder::from_device(cpal_device)?
                .with_error_callback(callback)
                .open_sink_or_fallback()?
        }
    };

    let config = stream.config();
    let output_config = OutputConfig {
        // Retain the stable identity and successful configuration, not a Device
        // handle or a description that may identify multiple audio endpoints.
        host,
        device_id: id,
        config: cpal::StreamConfig {
            channels: config.channel_count().get(),
            sample_rate: config.sample_rate().get(),
            buffer_size: *config.buffer_size(),
        },
        sample_format: config.sample_format(),
    };
    Ok((RodioOutput::new(stream, errors), output_config))
}

pub fn open(host: cpal::Host, device: Option<String>, format: AudioFormat) -> RodioSink {
    info!(
        "Using Rodio sink with format {format:?} and cpal host: {}",
        host.id().name()
    );

    let (output, output_config) = create_sink(host, device, format).unwrap();

    debug!("Rodio sink was created");
    RodioSink {
        output: Some(output),
        output_config,
        recovery: RecoveryPolicy::default(),
    }
}

impl Sink for RodioSink {
    fn start(&mut self) -> SinkResult<()> {
        // A user-requested resume can retry after the automatic recovery budget
        // was exhausted. Successful opens alone do not reset that budget.
        self.recovery = RecoveryPolicy::default();
        self.ensure_output()?;
        if let Some(output) = &self.output {
            output.sink.play();
        }
        Ok(())
    }

    fn stop(&mut self) -> SinkResult<()> {
        if self.output.is_none() {
            return Ok(());
        }
        match self.wait_for_queue(0) {
            Ok(()) => {
                if let Some(output) = &self.output {
                    output.sink.pause();
                }
            }
            Err(error) => {
                warn!("Discarding failed Rodio output while stopping: {error}");
                // The player exits on stop() errors. A failed output has
                // nothing left to drain; release it and allow a later start.
                self.output = None;
            }
        }
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        self.ensure_output()?;
        let samples = packet
            .samples()
            .map_err(|e| RodioError::Samples(e.to_string()))?;
        let samples_f32: &[f32] = &converter.f64_to_f32(samples);
        let source = rodio::buffer::SamplesBuffer::new(
            rodio::ChannelCount::new(NUM_CHANNELS as u16).expect("nonzero channel count"),
            rodio::SampleRate::new(SAMPLE_RATE).expect("nonzero sample rate"),
            samples_f32,
        );
        if let Some(output) = &mut self.output {
            output.sink.append(source);
            output.observed_queue_len = output.sink.len();
        }

        // Chunk sizes seem to be about 256 to 3000 ish items long.
        // Assuming they're on average 1628 then a half second buffer is:
        // 44100 elements --> about 27 chunks
        if let Err(error) = self.wait_for_queue(MAX_QUEUED_SOURCES) {
            // The failed queue may have been partly consumed. Discard it
            // instead of replaying already heard samples after reopening.
            self.recover(error)?;
        }
        Ok(())
    }
}

impl RodioSink {
    #[allow(dead_code)]
    pub const NAME: &'static str = "rodio";

    fn ensure_output(&mut self) -> Result<(), RodioError> {
        let error = match &mut self.output {
            Some(output) => {
                let error = output.error();
                if error.is_none() && output.queue_progress().1 {
                    self.recovery.record_progress(Instant::now());
                }
                error
            }
            None => Some(RodioError::OutputFailed("output is closed".to_string())),
        };
        if let Some(error) = error {
            self.recover(error)?;
        }
        Ok(())
    }

    fn recover(&mut self, error: RodioError) -> Result<(), RodioError> {
        warn!("Recovering Rodio output: {error}");
        // Drop on the owning player thread, before opening a replacement. The
        // stream can hold exclusive access to an ALSA playback substream.
        self.output = None;
        let output = self
            .recovery
            .retry(error, Instant::now, thread::sleep, || {
                self.output_config.open()
            })?;
        output.sink.play();
        self.output = Some(output);
        info!("Rodio output was reopened");
        Ok(())
    }

    fn wait_for_queue(&mut self, limit: usize) -> Result<(), RodioError> {
        let wait = QueueWait {
            started: Instant::now(),
        };
        loop {
            let output = self
                .output
                .as_mut()
                .ok_or_else(|| RodioError::OutputFailed("output is closed".to_string()))?;
            let error = output.error();
            let (queued, progressed) = output.queue_progress();
            let now = Instant::now();
            if error.is_none() && progressed {
                self.recovery.record_progress(now);
            }
            if wait.poll(now, queued, limit, error)? {
                return Ok(());
            }
            thread::sleep(QUEUE_POLL_INTERVAL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output_error() -> RodioError {
        RodioError::OutputFailed("simulated stream failure".to_string())
    }

    #[test]
    fn callback_storm_is_bounded_and_cannot_poison_a_replacement() {
        let (old_errors, mut old_callback) = stream_error_channel();
        old_callback(cpal::ErrorKind::DeviceNotAvailable.into());
        for _ in 0..1_000 {
            old_callback(cpal::Error::with_message(
                cpal::ErrorKind::StreamInvalidated,
                "recovery exhausted after repeated POLLERR",
            ));
        }
        assert_eq!(
            old_errors.try_recv().unwrap().kind(),
            cpal::ErrorKind::DeviceNotAvailable
        );
        assert!(matches!(old_errors.try_recv(), Err(TryRecvError::Empty)));

        let (new_errors, mut new_callback) = stream_error_channel();
        drop(old_errors);
        old_callback(cpal::ErrorKind::DeviceNotAvailable.into());
        assert!(matches!(new_errors.try_recv(), Err(TryRecvError::Empty)));
        new_callback(cpal::ErrorKind::DeviceNotAvailable.into());
        assert!(new_errors.try_recv().is_ok());
    }

    #[test]
    fn nonfatal_notifications_do_not_hide_a_terminal_error() {
        let (errors, mut callback) = stream_error_channel();
        for _ in 0..1_000 {
            for kind in [
                cpal::ErrorKind::Xrun,
                cpal::ErrorKind::RealtimeDenied,
                cpal::ErrorKind::DeviceChanged,
            ] {
                callback(kind.into());
            }
        }
        assert!(matches!(errors.try_recv(), Err(TryRecvError::Empty)));

        // A recovery budget exhausted by the patched ALSA worker is terminal,
        // even though the individual xruns preceding it were recoverable.
        callback(cpal::ErrorKind::StreamInvalidated.into());
        callback(cpal::ErrorKind::Xrun.into());
        assert_eq!(
            errors.try_recv().unwrap().kind(),
            cpal::ErrorKind::StreamInvalidated
        );
        assert!(matches!(errors.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn producer_and_drain_waits_abort_on_error_even_when_queue_is_empty() {
        let started = Instant::now();
        let wait = QueueWait { started };
        for limit in [0, MAX_QUEUED_SOURCES] {
            assert!(matches!(
                wait.poll(started, 0, limit, Some(output_error())),
                Err(RodioError::OutputFailed(_))
            ));
        }
    }

    #[test]
    fn producer_and_drain_waits_have_a_deadline_even_with_partial_progress() {
        let started = Instant::now();
        for limit in [0, MAX_QUEUED_SOURCES] {
            let wait = QueueWait { started };
            assert!(!wait.poll(started, limit + 3, limit, None).unwrap());
            assert!(
                !wait
                    .poll(started + Duration::from_secs(4), limit + 2, limit, None)
                    .unwrap()
            );
            assert!(matches!(
                wait.poll(started + QUEUE_WAIT_TIMEOUT, limit + 1, limit, None),
                Err(RodioError::OutputStalled)
            ));
        }
    }

    #[test]
    fn consumption_completes_producer_and_drain_waits() {
        let started = Instant::now();
        for limit in [0, MAX_QUEUED_SOURCES] {
            let wait = QueueWait { started };
            assert!(!wait.poll(started, limit + 1, limit, None).unwrap());
            assert!(
                wait.poll(started + QUEUE_POLL_INTERVAL, limit, limit, None)
                    .unwrap()
            );
        }
    }

    #[test]
    fn recovery_retries_failed_opens_with_backoff() {
        let mut policy = RecoveryPolicy::default();
        let now = Instant::now();
        let mut opens = 0;
        let mut delays = vec![];
        let output = policy
            .retry(
                output_error(),
                || now,
                |delay| delays.push(delay),
                || {
                    opens += 1;
                    if opens < 3 {
                        Err(output_error())
                    } else {
                        Ok("replacement")
                    }
                },
            )
            .unwrap();
        assert_eq!(output, "replacement");
        assert_eq!(opens, RECOVERY_BACKOFF.len());
        assert_eq!(delays, RECOVERY_BACKOFF);
        assert_eq!(policy.attempts, RECOVERY_BACKOFF.len());
    }

    #[test]
    fn repeatedly_failing_opens_exhaust_the_budget() {
        let mut policy = RecoveryPolicy::default();
        let now = Instant::now();
        let mut opens = 0;
        let result: Result<(), _> = policy.retry(
            output_error(),
            || now,
            |_| {},
            || {
                opens += 1;
                Err(output_error())
            },
        );
        assert!(matches!(result, Err(RodioError::RecoveryExhausted(_))));
        assert_eq!(opens, RECOVERY_BACKOFF.len());
    }

    #[test]
    fn opening_without_playback_progress_does_not_reset_recovery_budget() {
        let mut policy = RecoveryPolicy::default();
        let now = Instant::now();
        for _ in RECOVERY_BACKOFF {
            policy
                .retry(output_error(), || now, |_| {}, || Ok(()))
                .unwrap();
        }
        // Time passing and even successful opens must not permit an endless
        // close/reopen loop when the audio worker fails immediately each time.
        assert!(matches!(
            policy.retry(
                output_error(),
                || now + RECOVERY_RESET_AFTER,
                |_| panic!("exhausted recovery must not wait"),
                || -> Result<(), _> { panic!("exhausted recovery must not open") },
            ),
            Err(RodioError::RecoveryExhausted(_))
        ));
        policy.record_progress(now + RECOVERY_RESET_AFTER - Duration::from_millis(1));
        assert_eq!(policy.attempts, RECOVERY_BACKOFF.len());
        policy.record_progress(now + RECOVERY_RESET_AFTER);
        policy
            .retry(output_error(), || now, |_| {}, || Ok(()))
            .unwrap();
        assert_eq!(policy.attempts, 1);
    }

    // Run explicitly with the null PCM config; no hardware or Spotify session
    // is needed. The opt-in prevents accidentally opening a real default card.
    //
    // ALSA_CONFIG_PATH="$PWD/playback/tests/alsa-null.conf" \
    // LIBRESPOT_TEST_ALSA_NULL=1 cargo test -p librespot-playback --lib \
    // alsa_null_recovers_releases_and_reopens -- --ignored --test-threads=1
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires the explicit ALSA null PCM configuration"]
    fn alsa_null_recovers_releases_and_reopens() {
        require_null_pcm_config();
        let mut backend = open(
            cpal::host_from_id(cpal::HostId::Alsa).unwrap(),
            Some("librespot_test_null_a".to_owned()),
            AudioFormat::S16,
        );
        backend.start().unwrap();
        let config = backend.output_config.config;
        let sample_format = backend.output_config.sample_format;
        let device_id = backend.output_config.device_id.clone();
        assert_eq!(device_id.id(), "librespot_test_null_a");

        // Inject the callback notification into a live stream's mailbox. Its
        // actual CPAL stream still gets dropped/joined and reopened normally.
        let (errors, mut old_callback) = stream_error_channel();
        backend.output.as_mut().unwrap().errors = errors;
        for kind in [
            cpal::ErrorKind::Xrun,
            cpal::ErrorKind::RealtimeDenied,
            cpal::ErrorKind::DeviceChanged,
        ] {
            old_callback(kind.into());
            backend.ensure_output().unwrap();
            assert_eq!(backend.recovery.attempts, 0);
        }
        old_callback(cpal::ErrorKind::StreamInvalidated.into());
        backend.ensure_output().unwrap();
        assert_eq!(backend.recovery.attempts, 1);

        old_callback(cpal::ErrorKind::DeviceNotAvailable.into());
        backend.ensure_output().unwrap();
        assert_eq!(backend.recovery.attempts, 1);
        let output = backend.output.as_mut().unwrap();
        assert_eq!(
            output._stream.config().channel_count().get(),
            config.channels
        );
        assert_eq!(
            output._stream.config().sample_rate().get(),
            config.sample_rate
        );
        assert_eq!(output._stream.config().sample_format(), sample_format);
        assert_eq!(*output._stream.config().buffer_size(), config.buffer_size);
        output.sink.append(rodio::buffer::SamplesBuffer::new(
            rodio::ChannelCount::new(NUM_CHANNELS as u16).expect("nonzero channel count"),
            rodio::SampleRate::new(SAMPLE_RATE).expect("nonzero sample rate"),
            vec![0.0; SAMPLE_RATE as usize / 10 * NUM_CHANNELS as usize],
        ));
        backend.wait_for_queue(0).unwrap();

        // Stopping a failed, paused queue must release the stream instead of
        // waiting for consumption that can no longer happen.
        let output = backend.output.as_mut().unwrap();
        output.sink.pause();
        output.sink.append(rodio::buffer::SamplesBuffer::new(
            rodio::ChannelCount::new(NUM_CHANNELS as u16).expect("nonzero channel count"),
            rodio::SampleRate::new(SAMPLE_RATE).expect("nonzero sample rate"),
            vec![0.0; SAMPLE_RATE as usize / 10 * NUM_CHANNELS as usize],
        ));
        let (errors, mut callback) = stream_error_channel();
        output.errors = errors;
        callback(cpal::ErrorKind::DeviceNotAvailable.into());
        backend.stop().unwrap();
        assert!(backend.output.is_none());

        backend.start().unwrap();
        assert!(backend.output.is_some());
        assert_eq!(backend.output_config.config, config);
        assert_eq!(backend.output_config.sample_format, sample_format);
        assert_eq!(backend.output_config.device_id, device_id);
        backend.stop().unwrap();
    }

    #[cfg(target_os = "linux")]
    fn require_null_pcm_config() {
        use std::path::PathBuf;
        assert_eq!(
            std::env::var("LIBRESPOT_TEST_ALSA_NULL").as_deref(),
            Ok("1")
        );
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/alsa-null.conf");
        let configured = PathBuf::from(std::env::var_os("ALSA_CONFIG_PATH").unwrap());
        assert_eq!(
            configured.canonicalize().unwrap(),
            fixture.canonicalize().unwrap()
        );
    }

    // Requires ALSA_CONFIG_PATH=$PWD/playback/tests/alsa-null.conf and
    // LIBRESPOT_TEST_ALSA_NULL=1. The explicit configuration check prevents this
    // opt-in smoke test from accidentally playing through a real default card.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires the explicit ALSA null PCM configuration"]
    fn alsa_null_plays_and_reopens() {
        require_null_pcm_config();

        let host = cpal::host_from_id(cpal::HostId::Alsa).unwrap();
        let default = select_device(&host, "default").unwrap();
        let default_id = default.id().unwrap();
        assert_eq!(default_id.id(), "default");
        assert_eq!(
            select_device(&host, &default_id.to_string()).unwrap(),
            default
        );
        assert_eq!(
            select_device(&host, &device_name(&default).unwrap()).unwrap(),
            default
        );

        let named = select_device(&host, "librespot_test_null_a").unwrap();
        let named_id = named.id().unwrap();
        assert_eq!(select_device(&host, &named_id.to_string()).unwrap(), named);
        assert!(matches!(
            select_device(&host, "Librespot null test output"),
            Err(RodioError::AmbiguousDeviceName(_))
        ));
        assert!(matches!(
            select_device(&host, "missing_test_device"),
            Err(RodioError::DeviceNotAvailable(_))
        ));

        // Cover the implicit default, explicit default, named raw ID and full
        // CPAL ID. Each iteration drops its real ALSA stream before reopening.
        for requested in [
            None,
            Some("default".to_owned()),
            Some("librespot_test_null_a".to_owned()),
            Some(named_id.to_string()),
        ] {
            let (output, output_config) = create_sink(
                cpal::host_from_id(cpal::HostId::Alsa).unwrap(),
                requested,
                AudioFormat::S16,
            )
            .unwrap();
            let stream = &output._stream;
            assert_eq!(stream.config().sample_format(), cpal::SampleFormat::I16);
            assert_eq!(stream.config().channel_count().get(), NUM_CHANNELS as u16);
            assert_eq!(stream.config().sample_rate().get(), SAMPLE_RATE);
            assert!(matches!(
                stream.config().buffer_size(),
                cpal::BufferSize::Fixed(_)
            ));
            let mut sink = RodioSink {
                output: Some(output),
                output_config,
                recovery: RecoveryPolicy::default(),
            };
            sink.start().unwrap();
            sink.write(
                AudioPacket::Samples(vec![
                    0.25;
                    SAMPLE_RATE as usize / 100 * NUM_CHANNELS as usize
                ]),
                &mut Converter::new(None),
            )
            .unwrap();

            // Check consumption with a deadline before entering the existing
            // healthy stop/drain path; a paused-worker regression must fail.
            let deadline = Instant::now() + Duration::from_secs(2);
            while sink.output.as_ref().unwrap().sink.len() != 0 {
                assert!(
                    Instant::now() < deadline,
                    "ALSA null output did not consume audio"
                );
                thread::sleep(Duration::from_millis(1));
            }
            sink.stop().unwrap();
        }
    }
}
