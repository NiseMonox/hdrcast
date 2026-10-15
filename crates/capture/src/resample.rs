//! Converts whatever PCM the audio endpoint delivers into the engine's one format: interleaved stereo f32 at
//! 48 kHz (design.md section 2.4). Pure logic, shared by every backend.

/// Sample rate of the audio pipe.
pub const TARGET_SAMPLE_RATE: u32 = 48000;
/// Channels of the audio pipe.
pub const TARGET_CHANNELS: usize = 2;

/// How the samples of a capture buffer are encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    /// IEEE float samples (32 bit); otherwise signed integer PCM (8 bit is unsigned).
    pub float: bool,
}

impl PcmFormat {
    pub fn bytes_per_sample(&self) -> usize {
        usize::from(self.bits_per_sample / 8).max(1)
    }

    pub fn bytes_per_frame(&self) -> usize {
        self.bytes_per_sample() * usize::from(self.channels.max(1))
    }

    fn read_sample(&self, bytes: &[u8]) -> f32 {
        if self.float {
            return match bytes {
                [a, b, c, d, ..] => f32::from_le_bytes([*a, *b, *c, *d]),
                _ => 0.0,
            };
        }
        match (self.bits_per_sample, bytes) {
            (16, [a, b, ..]) => f32::from(i16::from_le_bytes([*a, *b])) / 32768.0,
            (24, [a, b, c, ..]) => (i32::from_le_bytes([0, *a, *b, *c]) >> 8) as f32 / 8388608.0,
            (32, [a, b, c, d, ..]) => i32::from_le_bytes([*a, *b, *c, *d]) as f32 / 2147483648.0,
            (8, [a, ..]) => (f32::from(*a) - 128.0) / 128.0,
            _ => 0.0,
        }
    }
}

/// Streaming converter from one [`PcmFormat`] to 48 kHz stereo f32.
///
/// Channels: mono is copied to both sides; of anything wider only front left / right are kept (a simple
/// front downmix: centre and surround are ignored).
///
/// Rate: linear interpolation, continuous across buffers (the last frame of a buffer is kept so the first
/// output of the next one interpolates from it, and the fractional read position carries over). Desktop
/// audio is 44.1 / 96 kHz only on unusual setups, and after AAC at 192 kbit/s a better resampler is not
/// audible.
#[derive(Debug, Clone)]
pub struct StereoConverter {
    format: PcmFormat,
    /// Source frames per output frame.
    step: f64,
    /// Read position of the next output frame, in source frames relative to the start of the next buffer
    /// (-1.0 is the frame kept in `last`).
    position: f64,
    last: [f32; TARGET_CHANNELS],
    stereo: Vec<f32>,
}

impl StereoConverter {
    pub fn new(format: PcmFormat) -> Self {
        Self {
            format,
            step: f64::from(format.sample_rate.max(1)) / f64::from(TARGET_SAMPLE_RATE),
            position: 0.0,
            last: [0.0; TARGET_CHANNELS],
            stereo: Vec::new(),
        }
    }

    pub fn format(&self) -> PcmFormat {
        self.format
    }

    /// Converts one capture buffer (whole frames of the source format) and appends the result to `out`.
    /// Returns the number of 48 kHz frames appended.
    pub fn convert(&mut self, input: &[u8], out: &mut Vec<f32>) -> usize {
        let channels = usize::from(self.format.channels.max(1));
        let sample_bytes = self.format.bytes_per_sample();
        let frame_bytes = sample_bytes * channels;
        let frames = input.len() / frame_bytes;
        if frames == 0 {
            return 0;
        }

        // 1. decode to stereo float at the source rate
        let resample = self.format.sample_rate != TARGET_SAMPLE_RATE;
        let mut decoded = std::mem::take(&mut self.stereo);
        decoded.clear();
        let stereo: &mut Vec<f32> = if resample { &mut decoded } else { &mut *out };
        stereo.reserve(frames * TARGET_CHANNELS);
        for frame in input.chunks_exact(frame_bytes).take(frames) {
            let left = self.format.read_sample(frame);
            let right = if channels == 1 {
                left
            } else {
                self.format.read_sample(&frame[sample_bytes..])
            };
            stereo.push(left);
            stereo.push(right);
        }
        if !resample {
            self.stereo = decoded;
            return frames;
        }

        // 2. linear resample to 48 kHz
        let before = out.len();
        let last_index = (frames - 1) as f64;
        while self.position <= last_index {
            let base = self.position.floor();
            let frac = (self.position - base) as f32;
            let i0 = base as isize; // -1 = the last frame of the previous buffer
            for c in 0..TARGET_CHANNELS {
                let a = if i0 < 0 {
                    self.last[c]
                } else {
                    decoded[i0 as usize * TARGET_CHANNELS + c]
                };
                let b = decoded
                    .get((i0 + 1) as usize * TARGET_CHANNELS + c)
                    .copied()
                    .unwrap_or(a);
                out.push(a + (b - a) * frac);
            }
            self.position += self.step;
        }
        self.position -= frames as f64;
        self.last = [
            decoded[(frames - 1) * TARGET_CHANNELS],
            decoded[(frames - 1) * TARGET_CHANNELS + 1],
        ];
        self.stereo = decoded;
        (out.len() - before) / TARGET_CHANNELS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn float_format(rate: u32, channels: u16) -> PcmFormat {
        PcmFormat {
            sample_rate: rate,
            channels,
            bits_per_sample: 32,
            float: true,
        }
    }

    fn bytes_of(samples: &[f32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn stereo_float_at_48k_passes_through_bit_exact() {
        let mut c = StereoConverter::new(float_format(48000, 2));
        let input = [0.25f32, -0.5, 1.0, -1.0, 0.0, 0.125];
        let mut out = Vec::new();
        assert_eq!(c.convert(&bytes_of(&input), &mut out), 3);
        assert_eq!(out, input);
    }

    #[test]
    fn mono_goes_to_both_sides_and_surround_keeps_the_front_pair() {
        let mut out = Vec::new();
        StereoConverter::new(float_format(48000, 1)).convert(&bytes_of(&[0.5, -0.5]), &mut out);
        assert_eq!(out, [0.5, 0.5, -0.5, -0.5]);

        out.clear();
        let surround = [
            0.1f32, 0.2, 0.9, 0.9, 0.9, 0.9, 0.3, 0.4, 0.9, 0.9, 0.9, 0.9,
        ];
        StereoConverter::new(float_format(48000, 6)).convert(&bytes_of(&surround), &mut out);
        assert_eq!(out, [0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn integer_pcm_is_scaled_to_unit_range() {
        let mut out = Vec::new();
        let f16 = PcmFormat {
            sample_rate: 48000,
            channels: 2,
            bits_per_sample: 16,
            float: false,
        };
        StereoConverter::new(f16).convert(&[0x00, 0x40, 0x00, 0xC0], &mut out);
        assert_eq!(out, [0.5, -0.5]);

        out.clear();
        let f24 = PcmFormat {
            sample_rate: 48000,
            channels: 2,
            bits_per_sample: 24,
            float: false,
        };
        StereoConverter::new(f24).convert(&[0x00, 0x00, 0x40, 0x00, 0x00, 0xC0], &mut out);
        assert_eq!(out, [0.5, -0.5]);

        out.clear();
        let f32i = PcmFormat {
            sample_rate: 48000,
            channels: 2,
            bits_per_sample: 32,
            float: false,
        };
        StereoConverter::new(f32i).convert(&[0, 0, 0, 0x40, 0, 0, 0, 0xC0], &mut out);
        assert_eq!(out, [0.5, -0.5]);

        out.clear();
        let f8 = PcmFormat {
            sample_rate: 48000,
            channels: 2,
            bits_per_sample: 8,
            float: false,
        };
        StereoConverter::new(f8).convert(&[192, 64], &mut out);
        assert_eq!(out, [0.5, -0.5]);
    }

    /// One second of a 1 kHz sine at `rate`, fed in uneven buffers the way loopback capture delivers it.
    fn resampled_sine(rate: u32) -> Vec<f32> {
        let mut c = StereoConverter::new(float_format(rate, 2));
        let source: Vec<f32> = (0..rate)
            .flat_map(|i| {
                let v = (2.0 * std::f64::consts::PI * 1000.0 * f64::from(i) / f64::from(rate)).sin()
                    as f32;
                [v, -v]
            })
            .collect();
        let mut out = Vec::new();
        let mut offset = 0;
        for (n, size) in [441usize, 1, 480, 3000, 7, 1024].iter().cycle().enumerate() {
            if offset >= source.len() {
                break;
            }
            let end = (offset + size * 2).min(source.len());
            let produced = c.convert(&bytes_of(&source[offset..end]), &mut out);
            assert!(
                produced <= (end - offset) / 2 * 48000 / rate as usize + 2,
                "buffer {n} produced {produced}"
            );
            offset = end;
        }
        out
    }

    #[test]
    fn resampling_keeps_the_duration_and_the_waveform_across_buffer_boundaries() {
        for rate in [44100u32, 96000, 32000] {
            let out = resampled_sine(rate);
            let frames = out.len() / 2;
            // One second in, one second out (the frame after the last input sample cannot be interpolated yet).
            assert!(
                (47998..=48000).contains(&frames),
                "{rate} Hz: {frames} frames"
            );
            let mut worst = 0f64;
            for (i, frame) in out.chunks_exact(2).enumerate() {
                let expected = (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 48000.0).sin();
                worst = worst.max((f64::from(frame[0]) - expected).abs());
                assert_eq!(frame[0], -frame[1]);
            }
            // Linear interpolation of 1 kHz: the error is bounded by the curvature between two source samples.
            assert!(worst < 0.01, "{rate} Hz: worst error {worst}");
        }
    }
}
