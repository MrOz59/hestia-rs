//! ALSA exposes speaker positions independently of channel count.
//! Open the system default so PipeWire/ALSA routing remains in effect.

use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, Ordering};

use alsa::pcm::{Access, ChmapPosition, Format, Frames, HwParams, PCM};
use alsa::{Direction, ValueOr};
use log::info;

use crate::audio_fifo::{MAX_CHANNELS, SAMPLE_RATE};
use crate::audio_playback::PcmReader;

const PERIOD_FRAMES: Frames = SAMPLE_RATE as Frames / 200;

pub struct SurroundOutput {
    pcm: PCM,
    order: Vec<usize>,
    period: usize,
}

impl SurroundOutput {
    pub fn open(requested: usize) -> Result<Self, String> {
        let mut error = String::new();
        for &channels in if requested == 8 { &[8, 6][..] } else { &[6][..] } {
            for format in [Format::float(), Format::s16()] {
                match Self::open_format(channels, format) {
                    Ok(output) => {
                        info!(
                            "audio output: ALSA default (host {requested} ch, device {channels} ch, {format:?}, {} frame period)",
                            output.period
                        );
                        return Ok(output);
                    }
                    Err(e) => error = e,
                }
            }
        }
        Err(error)
    }

    fn open_format(channels: usize, format: Format) -> Result<Self, String> {
        let pcm = PCM::new("default", Direction::Playback, true).map_err(|e| e.to_string())?;
        let (period, rate) = (|| -> Result<_, alsa::Error> {
            let hw = HwParams::any(&pcm)?;
            hw.set_access(Access::RWInterleaved)?;
            hw.set_format(format)?;
            hw.set_channels(channels as u32)?;
            hw.set_rate(SAMPLE_RATE, ValueOr::Nearest)?;
            hw.set_period_size_near(PERIOD_FRAMES, ValueOr::Nearest)?;
            hw.set_buffer_size_near(PERIOD_FRAMES * 4)?;
            pcm.hw_params(&hw)?;

            let actual = pcm.hw_params_current()?;
            let period = actual.get_period_size()?;
            let sw = pcm.sw_params_current()?;
            sw.set_start_threshold((period * 2).min(actual.get_buffer_size()?))?;
            sw.set_avail_min(period)?;
            pcm.sw_params(&sw)?;
            Ok((period as usize, actual.get_rate()?))
        })()
        .map_err(|e| e.to_string())?;

        if rate != SAMPLE_RATE || period == 0 {
            return Err(format!(
                "unexpected ALSA configuration: {rate} Hz, {period} frame period"
            ));
        }

        let map = pcm.get_chmap().map_err(|e| e.to_string())?;
        let positions = Vec::from(&map);
        let order =
            speaker_order(&positions, channels).ok_or_else(|| format!("unsupported {channels}-channel map: {map}"))?;
        Ok(Self { pcm, order, period })
    }

    pub fn play(&self, reader: &PcmReader, stop: &AtomicBool) -> Result<(), alsa::Error> {
        match self.pcm.io_f32() {
            Ok(io) => self.play_samples(reader, stop, |s| s, 0.0, |samples| io.writei(samples)),
            Err(_) => {
                let io = self.pcm.io_i16()?;
                self.play_samples(
                    reader,
                    stop,
                    |s| (s.clamp(-1.0, 1.0) * 32767.0) as i16,
                    0,
                    |samples| io.writei(samples),
                )
            }
        }
    }

    fn play_samples<T: Copy>(
        &self,
        reader: &PcmReader,
        stop: &AtomicBool,
        convert: impl Fn(f32) -> T,
        silence: T,
        write: impl Fn(&[T]) -> Result<usize, alsa::Error>,
    ) -> Result<(), alsa::Error> {
        let channels = self.order.len();
        let mut samples = vec![silence; self.period * channels];
        let mut written = self.period;
        while !stop.load(Ordering::Relaxed) {
            let avail = match self.pcm.wait(Some(20)) {
                Ok(true) => self.pcm.avail_update(),
                Ok(false) => continue,
                Err(e) => Err(e),
            };
            let avail = match avail {
                Ok(frames) if frames > 0 => frames as usize,
                Ok(_) => continue,
                Err(e) => {
                    self.pcm.try_recover(e, true)?;
                    written = self.period;
                    continue;
                }
            };
            if written == self.period {
                reader.read(&mut samples, channels, &convert, silence);
                let mut frame = [silence; MAX_CHANNELS];
                for samples in samples.chunks_exact_mut(channels) {
                    frame[..channels].copy_from_slice(samples);
                    for (dst, &index) in samples.iter_mut().zip(&self.order) {
                        *dst = frame[index];
                    }
                }
                written = 0;
            }
            let end = self.period.min(written + avail);
            match write(&samples[written * channels..end * channels]) {
                Ok(frames) => written += frames,
                Err(e) if std::io::Error::from_raw_os_error(e.errno()).kind() == ErrorKind::WouldBlock => continue,
                Err(e) => {
                    self.pcm.try_recover(e, true)?;
                    written = self.period;
                }
            }
        }
        Ok(())
    }
}

fn speaker_order(positions: &[ChmapPosition], channels: usize) -> Option<Vec<usize>> {
    use ChmapPosition::*;
    let mut order = Vec::with_capacity(channels);
    if positions.len() != channels {
        return None;
    }
    for position in positions {
        let index = match position {
            FL => 0,
            FR => 1,
            FC => 2,
            LFE => 3,
            RL => 4,
            RR => 5,
            SL => {
                if channels == 6 {
                    4
                } else {
                    6
                }
            }
            SR => {
                if channels == 6 {
                    5
                } else {
                    7
                }
            }
            _ => return None,
        };
        if order.contains(&index) {
            return None;
        }
        order.push(index);
    }
    Some(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ChmapPosition::*;

    #[test]
    fn maps_alsa_speakers_to_gamestream_order() {
        assert_eq!(
            speaker_order(&[FL, FR, RL, RR, FC, LFE, SL, SR], 8),
            Some(vec![0, 1, 4, 5, 2, 3, 6, 7])
        );
        assert_eq!(
            speaker_order(&[FL, FR, FC, LFE, SL, SR], 6),
            Some(vec![0, 1, 2, 3, 4, 5])
        );
        assert_eq!(speaker_order(&[FL, FR, RL, RR, FC, SL], 6), None);
    }
}
