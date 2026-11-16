use std::process::exit;
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait};
use thiserror::Error;

use super::{Sink, SinkError, SinkResult};
use crate::config::AudioFormat;
use crate::convert::Converter;
use crate::decoder::AudioPacket;
use crate::{NUM_CHANNELS, SAMPLE_RATE};

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
}

impl From<RodioError> for SinkError {
    fn from(e: RodioError) -> SinkError {
        use RodioError::*;
        let es = e.to_string();
        match e {
            StreamError(_) | PlayError(_) | Samples(_) => SinkError::OnWrite(es),
            NoDeviceAvailable | DeviceNotAvailable(_) => SinkError::ConnectionRefused(es),
            DevicesError(_) | AmbiguousDeviceName(_) => SinkError::InvalidParams(es),
        }
    }
}

pub struct RodioSink {
    rodio_sink: rodio::Player,
    _stream: rodio::MixerDeviceSink,
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
    host: &cpal::Host,
    device: Option<String>,
    format: AudioFormat,
) -> Result<(rodio::Player, rodio::MixerDeviceSink), RodioError> {
    let cpal_device = match device.as_deref() {
        Some("?") => match list_outputs(host) {
            Ok(()) => exit(0),
            Err(e) => {
                error!("{e}");
                exit(1);
            }
        },
        Some(requested) => select_device(host, requested)?,
        None => host
            .default_output_device()
            .ok_or(RodioError::NoDeviceAvailable)?,
    };

    let name = device_name(&cpal_device).ok();
    let id = cpal_device.id().ok().map(|id| id.to_string());
    info!(
        "Using audio device: {} ({})",
        id.as_deref().unwrap_or("[unknown ID]"),
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
    let builder = rodio::DeviceSinkBuilder::from_device(cpal_device.clone())?
        .with_channels(rodio::ChannelCount::new(config.channels()).expect("nonzero CPAL channels"))
        .with_sample_rate(rodio::SampleRate::new(config.sample_rate()).expect("nonzero CPAL rate"))
        .with_sample_format(sample_format);
    #[cfg(feature = "rodiojack-backend")]
    let builder = if host.id() == cpal::HostId::Jack {
        // JACK owns its period size; an unrelated fixed request is rejected.
        builder.with_buffer_size(cpal::BufferSize::Default)
    } else {
        builder
    };
    let mut stream = match builder.open_stream() {
        Ok(exact_stream) => exact_stream,
        Err(e) => {
            warn!("unable to create Rodio output, falling back to default: {e}");
            rodio::DeviceSinkBuilder::from_device(cpal_device)?.open_sink_or_fallback()?
        }
    };

    // disable logging on stream drop
    stream.log_on_drop(false);

    let sink = rodio::Player::connect_new(stream.mixer());
    Ok((sink, stream))
}

pub fn open(host: cpal::Host, device: Option<String>, format: AudioFormat) -> RodioSink {
    info!(
        "Using Rodio sink with format {format:?} and cpal host: {}",
        host.id().name()
    );

    let (sink, stream) = create_sink(&host, device, format).unwrap();

    debug!("Rodio sink was created");
    RodioSink {
        rodio_sink: sink,
        _stream: stream,
    }
}

impl Sink for RodioSink {
    fn start(&mut self) -> SinkResult<()> {
        self.rodio_sink.play();
        Ok(())
    }

    fn stop(&mut self) -> SinkResult<()> {
        self.rodio_sink.sleep_until_end();
        self.rodio_sink.pause();
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        let samples = packet
            .samples()
            .map_err(|e| RodioError::Samples(e.to_string()))?;
        let samples_f32: &[f32] = &converter.f64_to_f32(samples);
        let source = rodio::buffer::SamplesBuffer::new(
            rodio::ChannelCount::new(NUM_CHANNELS as u16).expect("nonzero channel count"),
            rodio::SampleRate::new(SAMPLE_RATE).expect("nonzero sample rate"),
            samples_f32,
        );
        self.rodio_sink.append(source);

        // Chunk sizes seem to be about 256 to 3000 ish items long.
        // Assuming they're on average 1628 then a half second buffer is:
        // 44100 elements --> about 27 chunks
        while self.rodio_sink.len() > 26 {
            // sleep and wait for rodio to drain a bit
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}

impl RodioSink {
    #[allow(dead_code)]
    pub const NAME: &'static str = "rodio";
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Instant;

    // Requires ALSA_CONFIG_PATH=$PWD/playback/tests/alsa-null.conf and
    // LIBRESPOT_TEST_ALSA_NULL=1. The explicit configuration check prevents this
    // opt-in smoke test from accidentally playing through a real default card.
    #[test]
    #[ignore = "requires the explicit ALSA null PCM configuration"]
    fn alsa_null_plays_and_reopens() {
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
            let (player, stream) = create_sink(&host, requested, AudioFormat::S16).unwrap();
            assert_eq!(stream.config().sample_format(), cpal::SampleFormat::I16);
            assert_eq!(stream.config().channel_count().get(), NUM_CHANNELS as u16);
            assert_eq!(stream.config().sample_rate().get(), SAMPLE_RATE);
            assert!(matches!(
                stream.config().buffer_size(),
                cpal::BufferSize::Fixed(_)
            ));
            let mut sink = RodioSink {
                rodio_sink: player,
                _stream: stream,
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
            while sink.rodio_sink.len() != 0 {
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
