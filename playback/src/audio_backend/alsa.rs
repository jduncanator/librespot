use super::{Open, Sink, SinkAsBytes, SinkError, SinkResult};
use crate::config::AudioFormat;
use crate::convert::Converter;
use crate::decoder::AudioPacket;
use crate::{NUM_CHANNELS, SAMPLE_RATE};
use alsa::device_name::HintIter;
use alsa::pcm::{Access, Format, Frames, HwParams, PCM};
use alsa::{Direction, ValueOr};
use std::process::exit;
use thiserror::Error;

mod io;

use io::{PlaybackIo, SystemClock, finish, write_pending};

const MAX_BUFFER: Frames = (SAMPLE_RATE / 2) as Frames;
const MIN_BUFFER: Frames = (SAMPLE_RATE / 10) as Frames;
const ZERO_FRAMES: Frames = 0;

const MAX_PERIOD_DIVISOR: Frames = 4;
const MIN_PERIOD_DIVISOR: Frames = 10;

#[derive(Debug, Error)]
enum AlsaError {
    #[error("<AlsaSink> Device {device} Unsupported Format {alsa_format:?} ({format:?}), {e}")]
    UnsupportedFormat {
        device: String,
        alsa_format: Format,
        format: AudioFormat,
        e: alsa::Error,
    },

    #[error("<AlsaSink> Device {device} Unsupported Channel Count {channel_count}, {e}")]
    UnsupportedChannelCount {
        device: String,
        channel_count: u8,
        e: alsa::Error,
    },

    #[error("<AlsaSink> Device {device} Unsupported Sample Rate {samplerate}, {e}")]
    UnsupportedSampleRate {
        device: String,
        samplerate: u32,
        e: alsa::Error,
    },

    #[error("<AlsaSink> Device {device} Unsupported Access Type RWInterleaved, {e}")]
    UnsupportedAccessType { device: String, e: alsa::Error },

    #[error("<AlsaSink> Device {device} May be Invalid, Busy, or Already in Use, {e}")]
    PcmSetUp { device: String, e: alsa::Error },

    #[error("<AlsaSink> Failed to Drain PCM Buffer, {0}")]
    DrainFailure(alsa::Error),

    #[error("<AlsaSink> {0}")]
    OnWrite(alsa::Error),

    #[error("<AlsaSink> Timed out waiting for PCM {0}")]
    Timeout(&'static str),

    #[error("<AlsaSink> Too many PCM recoveries without completing a write")]
    RecoveryLimit,

    #[error("<AlsaSink> Invalid PCM frame or period size")]
    InvalidFrameSize,

    #[error("<AlsaSink> PCM accepted more frames than supplied")]
    InvalidWriteSize,

    #[error("<AlsaSink> Hardware, {0}")]
    HwParams(alsa::Error),

    #[error("<AlsaSink> Software, {0}")]
    SwParams(alsa::Error),

    #[error("<AlsaSink> PCM, {0}")]
    Pcm(alsa::Error),

    #[error("<AlsaSink> Could Not Parse Output Name(s) and/or Description(s), {0}")]
    Parsing(alsa::Error),

    #[error("<AlsaSink>")]
    NotConnected,
}

impl From<AlsaError> for SinkError {
    fn from(e: AlsaError) -> SinkError {
        use AlsaError::*;
        let es = e.to_string();
        match e {
            DrainFailure(_) | OnWrite(_) | Timeout(_) | RecoveryLimit | InvalidWriteSize => {
                SinkError::OnWrite(es)
            }
            PcmSetUp { .. } => SinkError::ConnectionRefused(es),
            NotConnected => SinkError::NotConnected(es),
            _ => SinkError::InvalidParams(es),
        }
    }
}

impl From<AudioFormat> for Format {
    fn from(f: AudioFormat) -> Format {
        use AudioFormat::*;
        match f {
            F64 => Format::float64(),
            F32 => Format::float(),
            S32 => Format::s32(),
            S24 => Format::s24(),
            S24_3 => Format::s24_3(),
            S16 => Format::s16(),
        }
    }
}

pub struct AlsaSink {
    pcm: Option<PCM>,
    format: AudioFormat,
    device: String,
    period_buffer: Vec<u8>,
    period_bytes: usize,
    frame_bytes: usize,
}

fn list_compatible_devices() -> SinkResult<()> {
    let i = HintIter::new_str(None, "pcm").map_err(AlsaError::Parsing)?;

    println!("\n\n\tCompatible alsa device(s):\n");
    println!("\t------------------------------------------------------\n");

    for a in i {
        if let Some(Direction::Playback) = a.direction {
            if let Some(name) = a.name {
                if let Ok(pcm) = PCM::new(&name, Direction::Playback, false) {
                    if let Ok(hwp) = HwParams::any(&pcm) {
                        // Only show devices that support
                        // 2 ch 44.1 Interleaved.

                        if hwp.set_access(Access::RWInterleaved).is_ok()
                            && hwp.set_rate(SAMPLE_RATE, ValueOr::Nearest).is_ok()
                            && hwp.set_channels(NUM_CHANNELS as u32).is_ok()
                        {
                            let mut supported_formats = vec![];

                            for f in &[
                                AudioFormat::S16,
                                AudioFormat::S24,
                                AudioFormat::S24_3,
                                AudioFormat::S32,
                                AudioFormat::F32,
                                AudioFormat::F64,
                            ] {
                                if hwp.test_format(Format::from(*f)).is_ok() {
                                    supported_formats.push(format!("{f:?}"));
                                }
                            }

                            if !supported_formats.is_empty() {
                                println!("\tDevice:\n\n\t\t{name}\n");

                                println!(
                                    "\tDescription:\n\n\t\t{}\n",
                                    a.desc.unwrap_or_default().replace('\n', "\n\t\t")
                                );

                                println!(
                                    "\tSupported Format(s):\n\n\t\t{}\n",
                                    supported_formats.join(" ")
                                );

                                println!(
                                    "\t------------------------------------------------------\n"
                                );
                            }
                        }
                    };
                }
            }
        }
    }

    Ok(())
}

fn open_device(dev_name: &str, format: AudioFormat) -> SinkResult<(PCM, usize)> {
    // Blocking writes and drain can wait forever when a device's clock stops.
    // Keep the existing hardware/software parameters, but wait with a deadline ourselves.
    let pcm = PCM::new(dev_name, Direction::Playback, true).map_err(|e| AlsaError::PcmSetUp {
        device: dev_name.to_string(),
        e,
    })?;

    let bytes_per_period = {
        let hwp = HwParams::any(&pcm).map_err(AlsaError::HwParams)?;

        hwp.set_access(Access::RWInterleaved)
            .map_err(|e| AlsaError::UnsupportedAccessType {
                device: dev_name.to_string(),
                e,
            })?;

        let alsa_format = Format::from(format);

        hwp.set_format(alsa_format)
            .map_err(|e| AlsaError::UnsupportedFormat {
                device: dev_name.to_string(),
                alsa_format,
                format,
                e,
            })?;

        hwp.set_rate(SAMPLE_RATE, ValueOr::Nearest).map_err(|e| {
            AlsaError::UnsupportedSampleRate {
                device: dev_name.to_string(),
                samplerate: SAMPLE_RATE,
                e,
            }
        })?;

        hwp.set_channels(NUM_CHANNELS as u32)
            .map_err(|e| AlsaError::UnsupportedChannelCount {
                device: dev_name.to_string(),
                channel_count: NUM_CHANNELS,
                e,
            })?;

        // Clone the hwp while it's in
        // a good working state so that
        // in the event of an error setting
        // the buffer and period sizes
        // we can use the good working clone
        // instead of the hwp that's in an
        // error state.
        let hwp_clone = hwp.clone();

        // At a sampling rate of 44100:
        // The largest buffer is 22050 Frames (500ms) with 5512 Frame periods (125ms).
        // The smallest buffer is 4410 Frames (100ms) with 441 Frame periods (10ms).
        // Actual values may vary.
        //
        // Larger buffer and period sizes are preferred as extremely small values
        // will cause high CPU useage.
        //
        // If no buffer or period size is in those ranges or an error happens
        // trying to set the buffer or period size use the device's defaults
        // which may not be ideal but are *hopefully* serviceable.

        let buffer_size = {
            let max = match hwp.get_buffer_size_max() {
                Err(e) => {
                    trace!("Error getting the device's max Buffer size: {e}");
                    ZERO_FRAMES
                }
                Ok(s) => s,
            };

            let min = match hwp.get_buffer_size_min() {
                Err(e) => {
                    trace!("Error getting the device's min Buffer size: {e}");
                    ZERO_FRAMES
                }
                Ok(s) => s,
            };

            let buffer_size = if min < max {
                match (MIN_BUFFER..=MAX_BUFFER)
                    .rev()
                    .find(|f| (min..=max).contains(f))
                {
                    Some(size) => {
                        trace!("Desired Frames per Buffer: {size:?}");

                        match hwp.set_buffer_size_near(size) {
                            Err(e) => {
                                trace!("Error setting the device's Buffer size: {e}");
                                ZERO_FRAMES
                            }
                            Ok(s) => s,
                        }
                    }
                    None => {
                        trace!("No Desired Buffer size in range reported by the device.");
                        ZERO_FRAMES
                    }
                }
            } else {
                trace!(
                    "The device's min reported Buffer size was greater than or equal to its max reported Buffer size."
                );
                ZERO_FRAMES
            };

            if buffer_size == ZERO_FRAMES {
                trace!("Desired Buffer Frame range: {MIN_BUFFER:?} - {MAX_BUFFER:?}",);

                trace!("Actual Buffer Frame range as reported by the device: {min:?} - {max:?}",);
            }

            buffer_size
        };

        let period_size = {
            if buffer_size == ZERO_FRAMES {
                ZERO_FRAMES
            } else {
                let max = match hwp.get_period_size_max() {
                    Err(e) => {
                        trace!("Error getting the device's max Period size: {e}");
                        ZERO_FRAMES
                    }
                    Ok(s) => s,
                };

                let min = match hwp.get_period_size_min() {
                    Err(e) => {
                        trace!("Error getting the device's min Period size: {e}");
                        ZERO_FRAMES
                    }
                    Ok(s) => s,
                };

                let max_period = buffer_size / MAX_PERIOD_DIVISOR;
                let min_period = buffer_size / MIN_PERIOD_DIVISOR;

                let period_size = if min < max && min_period < max_period {
                    match (min_period..=max_period)
                        .rev()
                        .find(|f| (min..=max).contains(f))
                    {
                        Some(size) => {
                            trace!("Desired Frames per Period: {size:?}");

                            match hwp.set_period_size_near(size, ValueOr::Nearest) {
                                Err(e) => {
                                    trace!("Error setting the device's Period size: {e}");
                                    ZERO_FRAMES
                                }
                                Ok(s) => s,
                            }
                        }
                        None => {
                            trace!("No Desired Period size in range reported by the device.");
                            ZERO_FRAMES
                        }
                    }
                } else {
                    trace!(
                        "The device's min reported Period size was greater than or equal to its max reported Period size,"
                    );
                    trace!(
                        "or the desired min Period size was greater than or equal to the desired max Period size."
                    );
                    ZERO_FRAMES
                };

                if period_size == ZERO_FRAMES {
                    trace!("Buffer size: {buffer_size:?}");

                    trace!(
                        "Desired Period Frame range: {min_period:?} (Buffer size / {MIN_PERIOD_DIVISOR:?}) - {max_period:?} (Buffer size / {MAX_PERIOD_DIVISOR:?})",
                    );

                    trace!(
                        "Actual Period Frame range as reported by the device: {min:?} - {max:?}",
                    );
                }

                period_size
            }
        };

        if buffer_size == ZERO_FRAMES || period_size == ZERO_FRAMES {
            trace!(
                "Failed to set Buffer and/or Period size, falling back to the device's defaults."
            );

            trace!("You may experience higher than normal CPU usage and/or audio issues.");

            pcm.hw_params(&hwp_clone).map_err(AlsaError::Pcm)?;
        } else {
            pcm.hw_params(&hwp).map_err(AlsaError::Pcm)?;
        }

        let hwp = pcm.hw_params_current().map_err(AlsaError::Pcm)?;

        // Don't assume we got what we wanted. Ask to make sure.
        let frames_per_period = hwp.get_period_size().map_err(AlsaError::HwParams)?;

        let frames_per_buffer = hwp.get_buffer_size().map_err(AlsaError::HwParams)?;

        let swp = pcm.sw_params_current().map_err(AlsaError::Pcm)?;

        swp.set_start_threshold(frames_per_buffer - frames_per_period)
            .map_err(AlsaError::SwParams)?;

        pcm.sw_params(&swp).map_err(AlsaError::Pcm)?;

        trace!("Actual Frames per Buffer: {frames_per_buffer:?}");
        trace!("Actual Frames per Period: {frames_per_period:?}");

        // Let ALSA do the math for us.
        pcm.frames_to_bytes(frames_per_period) as usize
    };

    trace!("Period Buffer size in bytes: {bytes_per_period:?}");

    Ok((pcm, bytes_per_period))
}

impl Open for AlsaSink {
    fn open(device: Option<String>, format: AudioFormat) -> Self {
        let name = match device.as_deref() {
            Some("?") => match list_compatible_devices() {
                Ok(_) => {
                    exit(0);
                }
                Err(e) => {
                    error!("{e}");
                    exit(1);
                }
            },
            Some(device) => device,
            None => "default",
        }
        .to_string();

        info!("Using AlsaSink with format: {format:?}");

        Self {
            pcm: None,
            format,
            device: name,
            period_buffer: vec![],
            period_bytes: 0,
            frame_bytes: 0,
        }
    }
}

impl Sink for AlsaSink {
    fn start(&mut self) -> SinkResult<()> {
        if self.pcm.is_none() {
            let (pcm, bytes_per_period) = open_device(&self.device, self.format)?;
            let bytes_per_frame = pcm.frames_to_bytes(1);
            if bytes_per_frame <= 0
                || bytes_per_period == 0
                || bytes_per_period % bytes_per_frame as usize != 0
            {
                return Err(AlsaError::InvalidFrameSize.into());
            }
            self.pcm = Some(pcm);
            self.period_bytes = bytes_per_period;
            self.frame_bytes = bytes_per_frame as usize;

            self.period_buffer.clear();
            if self.period_buffer.capacity() < bytes_per_period {
                self.period_buffer = Vec::with_capacity(bytes_per_period);
            }

            // Allocation capacity can exceed the negotiated period size; writes
            // use period_bytes rather than treating capacity as an audio setting.
            trace!(
                "Period Buffer capacity: {:?}",
                self.period_buffer.capacity()
            );
        }

        Ok(())
    }

    fn stop(&mut self) -> SinkResult<()> {
        if let Err(e) = finish(
            &mut self.pcm,
            &mut self.period_buffer,
            self.period_bytes,
            self.frame_bytes,
            &mut SystemClock::default(),
        ) {
            // The device has been closed even if its remaining audio could not be
            // drained. Reporting a stop error makes the player exit the process;
            // a discarded tail should instead allow a later start to reopen it.
            warn!("AlsaSink closed without draining all queued audio: {e}");
        }

        Ok(())
    }

    sink_as_bytes!();
}

impl SinkAsBytes for AlsaSink {
    #[inline]
    fn write_bytes(&mut self, data: &[u8]) -> SinkResult<()> {
        if self.pcm.is_none() {
            return Err(AlsaError::NotConnected.into());
        }

        let mut start_index = 0;
        let data_len = data.len();
        let capacity = self.period_bytes;

        loop {
            let data_left = data_len - start_index;
            let space_left = capacity - self.period_buffer.len();
            let data_to_buffer = data_left.min(space_left);
            let end_index = start_index + data_to_buffer;

            self.period_buffer
                .extend_from_slice(&data[start_index..end_index]);

            if self.period_buffer.len() == capacity {
                self.write_buf()?;
            }

            if end_index == data_len {
                break Ok(());
            }

            start_index = end_index;
        }
    }
}

impl AlsaSink {
    pub const NAME: &'static str = "alsa";

    fn write_buf(&mut self) -> SinkResult<()> {
        write_pending(
            &mut self.pcm,
            &mut self.period_buffer,
            self.frame_bytes,
            &mut SystemClock::default(),
        )
        .map_err(Into::into)
    }
}

impl PlaybackIo for PCM {
    fn write(&mut self, data: &[u8]) -> Result<usize, alsa::Error> {
        self.io_bytes().writei(data)
    }

    fn wait(&mut self, timeout_ms: u32) -> Result<bool, alsa::Error> {
        PCM::wait(self, Some(timeout_ms))
    }

    fn prepare(&mut self) -> Result<(), alsa::Error> {
        PCM::prepare(self)
    }

    fn resume(&mut self) -> Result<(), alsa::Error> {
        PCM::resume(self)
    }

    fn drain(&mut self) -> Result<(), alsa::Error> {
        PCM::drain(self)
    }

    fn discard(&mut self) -> Result<(), alsa::Error> {
        PCM::drop(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writing_before_start_returns_not_connected() {
        let mut sink = AlsaSink::open(None, AudioFormat::S16);
        assert!(matches!(
            sink.write_bytes(&[0; 4]),
            Err(SinkError::NotConnected(_))
        ));
        assert!(sink.period_buffer.is_empty());
        sink.stop().unwrap();
    }

    #[test]
    #[ignore = "requires the ALSA null PCM plugin"]
    fn null_pcm_closes_after_write_failure_and_can_restart() {
        let mut sink = AlsaSink::open(Some("null".to_string()), AudioFormat::S16);
        sink.start().unwrap();
        let data = vec![0; sink.period_bytes];
        sink.write_bytes(&data).unwrap();

        // Drop puts the PCM in SETUP. The next write returns EBADFD rather than
        // accepting data, exercising the actual handle's terminal-error path.
        sink.pcm.as_ref().unwrap().drop().unwrap();
        assert!(sink.write_bytes(&data).is_err());
        assert!(sink.pcm.is_none());
        assert!(sink.period_buffer.is_empty());
        sink.stop().unwrap();

        // An unconfigured PCM cannot drain. stop must still close it and return
        // success, because a stop error makes the player exit the process.
        let unprepared = PCM::new("null", Direction::Playback, true).unwrap();
        assert!(unprepared.drain().is_err());
        sink.pcm = Some(unprepared);
        sink.stop().unwrap();
        assert!(sink.pcm.is_none());

        sink.start().unwrap();
        sink.write_bytes(&data).unwrap();
        sink.write_bytes(&[0; 4]).unwrap();
        sink.stop().unwrap();
        assert!(sink.pcm.is_none());
        assert!(sink.period_buffer.is_empty());
    }
}
