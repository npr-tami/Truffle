//! Decodes a whole m4a file (AAC, as YouTube serves it) from memory with symphonia, into the
//! interleaved stereo f64 packets Truffle's output plays.
//!
//! - The file's own length is not used: YouTube's (fragmented) files often say 0. The caller
//!   passes the length it knows (YouTube's, or else Spotify's).
//! - Mono is played on both channels; more than two channels are mixed down to stereo.
//! - Seeks land on the exact sample: the decoder starts a little before and the samples ahead of
//!   the target are dropped. AAC frames overlap, so the frame before the target is decoded too
//!   (and not played): without it the first frame after a jump comes out distorted.

use std::io::{Cursor, ErrorKind};

use anyhow::{Context, Result, anyhow, bail};
use symphonia::core::audio::{Channels, SampleBuffer};
use symphonia::core::codecs::{CODEC_TYPE_AAC, Decoder, DecoderOptions};
use symphonia::core::errors::{Error, SeekErrorKind};
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::{Time, TimeBase};

use super::pcm::PcmDecoder;

/// Bad packets in a row after which the file counts as broken.
const MAX_BAD_PACKETS: u32 = 50;
/// A seek starts decoding this long before its target (two AAC frames).
const PREROLL_MS: u32 = 50;

pub(super) struct M4aDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track: u32,
    time_base: TimeBase,
    rate: u32,
    duration_ms: u32,
    /// How each channel goes into the left and right output, for `mix_channels`.
    mix: Vec<(f64, f64)>,
    mix_channels: Channels,
    buffer: Option<SampleBuffer<f32>>,
    /// After a seek: samples before this timestamp are not played.
    skip_to: u64,
    /// Where the audio after the last packet starts, in ms.
    position_ms: u32,
    /// Seeked past the end of the audio.
    at_end: bool,
}

impl M4aDecoder {
    /// Opens the file. `duration_ms` is the song's length (0 if unknown).
    pub(super) fn open(bytes: Vec<u8>, duration_ms: u32) -> Result<Self> {
        let source = MediaSourceStream::new(Box::new(Cursor::new(bytes)), MediaSourceStreamOptions::default());
        let mut hint = Hint::new();
        hint.with_extension("m4a");
        let format = symphonia::default::get_probe()
            .format(&hint, source, &FormatOptions::default(), &MetadataOptions::default())
            .context("not an m4a file")?
            .format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec == CODEC_TYPE_AAC)
            .ok_or_else(|| anyhow!("no AAC audio in the file"))?;
        let params = &track.codec_params;
        let rate = params.sample_rate.filter(|&r| r > 0).ok_or_else(|| anyhow!("no sample rate"))?;
        let time_base = params.time_base.unwrap_or_else(|| TimeBase::new(1, rate));
        let mix_channels = params.channels.unwrap_or(Channels::FRONT_LEFT | Channels::FRONT_RIGHT);
        let decoder = symphonia::default::get_codecs()
            .make(params, &DecoderOptions::default())
            .context("can't decode this audio")?;
        let track = track.id;
        Ok(Self {
            format,
            decoder,
            track,
            time_base,
            rate,
            duration_ms,
            mix: stereo_mix(mix_channels),
            mix_channels,
            buffer: None,
            skip_to: 0,
            position_ms: 0,
            at_end: false,
        })
    }

    #[cfg(test)]
    pub(super) fn duration_ms(&self) -> u32 {
        self.duration_ms
    }

    fn ms(&self, ts: u64) -> u32 {
        let time = self.time_base.calc_time(ts);
        let ms = time.seconds.saturating_mul(1000).saturating_add((time.frac * 1000.0).round() as u64);
        u32::try_from(ms).unwrap_or(u32::MAX)
    }

    /// Interleaved stereo from the decoded buffer, without its first `skip` frames.
    fn stereo(&self, samples: &[f32], skip: usize) -> Vec<f64> {
        let channels = self.mix.len().max(1);
        let frames = samples.chunks_exact(channels).skip(skip);
        let mut out = Vec::with_capacity(frames.len() * 2);
        for frame in frames {
            let (mut left, mut right) = (0.0, 0.0);
            for (&sample, &(to_left, to_right)) in frame.iter().zip(&self.mix) {
                left += f64::from(sample) * to_left;
                right += f64::from(sample) * to_right;
            }
            out.push(left);
            out.push(right);
        }
        out
    }
}

impl PcmDecoder for M4aDecoder {
    fn rate(&self) -> u32 {
        self.rate
    }

    fn position_ms(&self) -> u32 {
        self.position_ms
    }

    fn seek(&mut self, position_ms: u32) -> Result<u32> {
        let target = if self.duration_ms > 0 { position_ms.min(self.duration_ms) } else { position_ms };
        let ts = |ms: u32| self.time_base.calc_timestamp(Time::from(f64::from(ms) / 1000.0));
        let target_ts = ts(target);
        let to = SeekTo::TimeStamp { ts: ts(target.saturating_sub(PREROLL_MS)), track_id: self.track };
        match self.format.seek(SeekMode::Accurate, to) {
            Ok(_) => {
                self.decoder.reset();
                self.skip_to = target_ts;
                self.position_ms = self.ms(target_ts);
                self.at_end = false;
            }
            // Past the last sample (the length given was longer than the audio): the song is over.
            Err(Error::SeekError(SeekErrorKind::OutOfRange)) => {
                self.position_ms = target;
                self.at_end = true;
            }
            Err(Error::IoError(e)) if e.kind() == ErrorKind::UnexpectedEof => {
                self.position_ms = target;
                self.at_end = true;
            }
            Err(e) => bail!("couldn't seek to {target} ms: {e}"),
        }
        Ok(self.position_ms)
    }

    fn next_packet(&mut self) -> Result<Option<Vec<f64>>> {
        if self.at_end {
            return Ok(None);
        }
        let mut bad = 0;
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(Error::IoError(e)) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
                Err(e) => bail!("couldn't read the audio: {e}"),
            };
            if packet.track_id() != self.track {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(decoded) => decoded,
                // A damaged packet: skip it, like a player would.
                Err(Error::DecodeError(e)) => {
                    bad += 1;
                    if bad >= MAX_BAD_PACKETS {
                        bail!("the audio is damaged: {e}");
                    }
                    continue;
                }
                Err(e) => bail!("couldn't decode the audio: {e}"),
            };
            // Before the target of a seek: decoded only so that the next frame comes out right.
            if decoded.frames() == 0 || packet.ts() + packet.dur() <= self.skip_to {
                continue;
            }
            let spec = *decoded.spec();
            if spec.channels != self.mix_channels {
                self.mix = stereo_mix(spec.channels);
                self.mix_channels = spec.channels;
            }
            let buffer = match self.buffer.as_mut() {
                Some(buffer) if buffer.capacity() >= decoded.capacity() * spec.channels.count() => buffer,
                _ => self.buffer.insert(SampleBuffer::new(decoded.capacity() as u64, spec)),
            };
            buffer.copy_interleaved_ref(decoded);
            let skip = if packet.ts() < self.skip_to {
                let ahead = self.time_base.calc_time(self.skip_to - packet.ts());
                ((ahead.seconds as f64 + ahead.frac) * f64::from(self.rate)).round() as usize
            } else {
                0
            };
            self.skip_to = 0;
            self.position_ms = self.ms(packet.ts() + packet.dur());
            let samples = self.buffer.as_ref().map_or(&[][..], SampleBuffer::samples);
            let out = self.stereo(samples, skip);
            if out.is_empty() {
                continue;
            }
            return Ok(Some(out));
        }
    }
}

/// How much of each channel (in the file's order) goes to the left and the right output.
fn stereo_mix(channels: Channels) -> Vec<(f64, f64)> {
    let count = channels.count();
    if count <= 1 {
        return vec![(1.0, 1.0)];
    }
    if count == 2 {
        return vec![(1.0, 0.0), (0.0, 1.0)];
    }
    let left = Channels::FRONT_LEFT
        | Channels::REAR_LEFT
        | Channels::FRONT_LEFT_CENTRE
        | Channels::SIDE_LEFT
        | Channels::TOP_FRONT_LEFT
        | Channels::TOP_REAR_LEFT
        | Channels::REAR_LEFT_CENTRE
        | Channels::FRONT_LEFT_WIDE
        | Channels::FRONT_LEFT_HIGH;
    let right = Channels::FRONT_RIGHT
        | Channels::REAR_RIGHT
        | Channels::FRONT_RIGHT_CENTRE
        | Channels::SIDE_RIGHT
        | Channels::TOP_FRONT_RIGHT
        | Channels::TOP_REAR_RIGHT
        | Channels::REAR_RIGHT_CENTRE
        | Channels::FRONT_RIGHT_WIDE
        | Channels::FRONT_RIGHT_HIGH;
    let lfe = Channels::LFE1 | Channels::LFE2;
    let centre = std::f64::consts::FRAC_1_SQRT_2;
    let mut mix: Vec<(f64, f64)> = channels
        .iter()
        .map(|channel| {
            if left.contains(channel) {
                (1.0, 0.0)
            } else if right.contains(channel) {
                (0.0, 1.0)
            } else if lfe.contains(channel) {
                (0.0, 0.0)
            } else {
                (centre, centre)
            }
        })
        .collect();
    // Scaled so that all channels at full level can't clip.
    let loudest = mix.iter().fold((0.0, 0.0), |(l, r), &(a, b)| (l + a, r + b));
    let scale = 1.0 / loudest.0.max(loudest.1).max(1.0);
    for weights in &mut mix {
        weights.0 *= scale;
        weights.1 *= scale;
    }
    mix
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sample file ("TV Girl - For You", YouTube format 140) at `TRUFFLE_TEST_DIR`\test.m4a, if there.
    fn sample_path() -> std::path::PathBuf {
        crate::config::test_root().join("test.m4a")
    }
    const SAMPLE_MS: u32 = 217_000;

    fn sample() -> Option<M4aDecoder> {
        let Ok(bytes) = std::fs::read(sample_path()) else {
            eprintln!("{} is missing; skipped", sample_path().display());
            return None;
        };
        Some(M4aDecoder::open(bytes, SAMPLE_MS).expect("the sample opens"))
    }

    fn peak(samples: &[f64]) -> f64 {
        samples.iter().fold(0.0, |m, s| m.max(s.abs()))
    }

    #[test]
    fn decodes_the_sample_file() {
        let Some(mut decoder) = sample() else { return };
        assert_eq!(decoder.rate(), 44_100);
        assert_eq!(decoder.duration_ms(), SAMPLE_MS);
        let mut frames = 0u64;
        let mut loudest = 0.0f64;
        let mut biggest_packet = 0;
        while let Some(packet) = decoder.next_packet().expect("decodes") {
            assert_eq!(packet.len() % 2, 0, "stereo");
            biggest_packet = biggest_packet.max(packet.len() / 2);
            frames += packet.len() as u64 / 2;
            loudest = loudest.max(peak(&packet));
        }
        let seconds = frames as f64 / 44_100.0;
        assert!((seconds - 217.0).abs() < 2.0, "{seconds} s of audio");
        assert!(loudest > 0.1 && loudest <= 1.5, "peak {loudest}");
        assert!(biggest_packet <= 2048, "packets of {biggest_packet} frames");
        let end = decoder.position_ms();
        assert!(end.abs_diff(217_000) < 2_000, "ends at {end} ms");
        // At the end it stays at the end.
        assert!(decoder.next_packet().unwrap().is_none());
        // And it can start again from there (repeat one).
        assert_eq!(decoder.seek(0).unwrap(), 0);
        assert!(decoder.next_packet().unwrap().is_some_and(|packet| !packet.is_empty()));
    }

    #[test]
    fn seeks_land_on_time() {
        let Some(mut decoder) = sample() else { return };
        // Decode a little first, so the seek starts from a used decoder.
        for _ in 0..20 {
            decoder.next_packet().unwrap();
        }
        let landed = decoder.seek(100_000).expect("seeks");
        assert!(landed.abs_diff(100_000) <= 50, "landed at {landed} ms");
        let packet = decoder.next_packet().unwrap().expect("audio after the seek");
        let after = decoder.position_ms();
        let expected = 100_000 + (packet.len() / 2) as u64 * 1000 / 44_100;
        assert!(u64::from(after).abs_diff(expected) <= 50, "{after} ms after one packet, expected {expected}");
        assert!(peak(&packet) > 0.0, "not silent in the middle of the song");
        // Backwards works too.
        let landed = decoder.seek(10_000).unwrap();
        assert!(landed.abs_diff(10_000) <= 50, "landed at {landed} ms");
        assert!(decoder.next_packet().unwrap().is_some());
        // Past the end is clamped, and the song is then over.
        let landed = decoder.seek(500_000).unwrap();
        assert!(landed <= SAMPLE_MS, "landed at {landed} ms");
        let mut left = 0;
        while decoder.next_packet().unwrap().is_some() {
            left += 1;
            assert!(left < 200, "still decoding long after the end");
        }
    }

    #[test]
    fn a_seek_sounds_the_same_as_playing_up_to_there() {
        let Some(mut through) = sample() else { return };
        // Times that are a whole number of frames, so both ways start on the same sample.
        let targets = [10_000u32, 37_250, 100_500, 150_000];
        let starts: Vec<usize> = targets.iter().map(|&ms| ms as usize * 441 / 10).collect();
        // The first 2048 frames from each time, playing through from the start.
        let mut want = vec![Vec::new(); targets.len()];
        let mut frame = 0;
        while let Some(packet) = through.next_packet().unwrap() {
            let frames = packet.len() / 2;
            for (want, &start) in want.iter_mut().zip(&starts) {
                let (from, to) = (start.max(frame), (start + 2048).min(frame + frames));
                if from < to {
                    want.extend_from_slice(&packet[(from - frame) * 2..(to - frame) * 2]);
                }
            }
            frame += frames;
        }
        for (ms, want) in targets.into_iter().zip(want) {
            let mut decoder = sample().unwrap();
            assert_eq!(decoder.seek(ms).unwrap(), ms);
            let mut got = Vec::new();
            while got.len() < want.len() {
                got.extend(decoder.next_packet().unwrap().expect("audio after the seek"));
            }
            let worst = want.iter().zip(&got).fold(0.0f64, |m, (a, b)| m.max((a - b).abs()));
            assert!(worst < 1e-4, "after a seek to {ms} ms the sound is off by up to {worst}");
        }
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(M4aDecoder::open(Vec::new(), 0).is_err());
        assert!(M4aDecoder::open(vec![7; 10_000], 0).is_err());
    }

    #[test]
    fn channels_mix_down_to_stereo() {
        assert_eq!(stereo_mix(Channels::FRONT_LEFT), vec![(1.0, 1.0)]);
        assert_eq!(stereo_mix(Channels::FRONT_LEFT | Channels::FRONT_RIGHT), vec![(1.0, 0.0), (0.0, 1.0)]);
        let surround = Channels::FRONT_LEFT
            | Channels::FRONT_RIGHT
            | Channels::FRONT_CENTRE
            | Channels::LFE1
            | Channels::REAR_LEFT
            | Channels::REAR_RIGHT;
        let mix = stereo_mix(surround);
        assert_eq!(mix.len(), 6);
        assert_eq!(mix[3], (0.0, 0.0), "the bass channel is left out");
        assert!(mix[2].0 > 0.0 && mix[2].0 == mix[2].1, "the centre goes to both sides");
        assert!(mix[0].1 == 0.0 && mix[1].0 == 0.0 && mix[4].1 == 0.0 && mix[5].0 == 0.0);
        let left: f64 = mix.iter().map(|m| m.0).sum();
        assert!((left - 1.0).abs() < 1e-9, "full scale at most: {left}");
    }
}
