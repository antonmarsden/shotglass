//! Audio output and the master clock.
//!
//! While playing, *the sound card decides what time it is*: the picture asks the
//! clock for the time of the sample being heard right now, so a slow frame becomes a
//! skipped frame instead of a slow-motion drift out of sync. Without an audio device
//! the clock falls back to the system timer.

use arc_swap::ArcSwap;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FRAC: f64 = 4294967296.0; // 32.32 fixed point sample position

struct Shared {
    samples: ArcSwap<Vec<f32>>, // interleaved stereo
    pos: AtomicU64,
    playing: AtomicBool,
    rate: AtomicU32,
    gain: AtomicU32,
    /// (position at the start of the last callback, when it ran, output latency)
    last: Mutex<Option<(u64, Instant, Duration)>>,
}

pub struct AudioOut {
    shared: Arc<Shared>,
    pub sample_rate: u32,
    pub device_name: String,
    _stream: cpal::Stream,
}

impl AudioOut {
    pub fn open() -> anyhow::Result<AudioOut> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow::anyhow!("no audio output device"))?;
        let device_name = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "audio device".into());
        let config = device.default_output_config()?;
        let sample_rate = config.sample_rate();
        let shared = Arc::new(Shared {
            samples: ArcSwap::from_pointee(vec![]),
            pos: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            rate: AtomicU32::new(1f32.to_bits()),
            gain: AtomicU32::new(1f32.to_bits()),
            last: Mutex::new(None),
        });
        let fmt = config.sample_format();
        let cfg: cpal::StreamConfig = config.into();
        let stream = match fmt {
            cpal::SampleFormat::F32 => build::<f32>(&device, cfg, shared.clone())?,
            cpal::SampleFormat::I16 => build::<i16>(&device, cfg, shared.clone())?,
            cpal::SampleFormat::U16 => build::<u16>(&device, cfg, shared.clone())?,
            cpal::SampleFormat::I32 => build::<i32>(&device, cfg, shared.clone())?,
            cpal::SampleFormat::F64 => build::<f64>(&device, cfg, shared.clone())?,
            other => anyhow::bail!("unsupported audio sample format {other}"),
        };
        stream.play()?;
        Ok(AudioOut {
            shared,
            sample_rate,
            device_name,
            _stream: stream,
        })
    }
}

fn build<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    cfg: cpal::StreamConfig,
    sh: Arc<Shared>,
) -> anyhow::Result<cpal::Stream> {
    let channels = cfg.channels as usize;
    let stream = device.build_output_stream(
        cfg,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let ts = info.timestamp();
            let latency = ts.playback - ts.callback;
            let playing = sh.playing.load(Ordering::Acquire);
            let start = sh.pos.load(Ordering::Acquire);
            let mut pos = start;
            if let Ok(mut l) = sh.last.try_lock() {
                *l = if playing {
                    Some((pos, Instant::now(), latency))
                } else {
                    None
                };
            }
            if !playing {
                data.iter_mut().for_each(|s| *s = T::from_sample(0.0f32));
                return;
            }
            let samples = sh.samples.load();
            let rate = f32::from_bits(sh.rate.load(Ordering::Relaxed)) as f64;
            let gain = f32::from_bits(sh.gain.load(Ordering::Relaxed));
            let step = (rate * FRAC) as u64;
            let frames = samples.len() / 2;
            for frame in data.chunks_mut(channels) {
                let i = (pos >> 32) as usize;
                let (l, r) = if i < frames {
                    (samples[i * 2] * gain, samples[i * 2 + 1] * gain)
                } else {
                    (0.0, 0.0)
                };
                for (c, s) in frame.iter_mut().enumerate() {
                    *s = T::from_sample(match c {
                        0 => l,
                        1 => r,
                        _ => 0.0,
                    });
                }
                if channels == 1 {
                    frame[0] = T::from_sample((l + r) * 0.5);
                }
                pos += step;
            }
            // Only advance if nobody seeked meanwhile (a seek wins).
            let _ = sh
                .pos
                .compare_exchange(start, pos, Ordering::AcqRel, Ordering::Relaxed);
        },
        |e| eprintln!("audio: {e}"),
        None,
    )?;
    Ok(stream)
}

pub struct Clock {
    audio: Option<AudioOut>,
    playing: bool,
    rate: f32,
    /// Time while paused; the time at the last play/seek while playing.
    floor: f32,
    /// When the last play/seek happened.
    since: Instant,
}

impl Clock {
    pub fn new(audio: Option<AudioOut>) -> Clock {
        Clock {
            audio,
            playing: false,
            rate: 1.0,
            floor: 0.0,
            since: Instant::now(),
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.audio.as_ref().map_or(44100, |a| a.sample_rate)
    }

    pub fn set_samples(&mut self, samples: Vec<f32>) {
        if let Some(a) = &self.audio {
            a.shared.samples.store(Arc::new(samples));
        }
    }

    pub fn playing(&self) -> bool {
        self.playing
    }

    pub fn rate(&self) -> f32 {
        self.rate
    }

    /// The movie time being *heard* right now.
    pub fn time(&self) -> f32 {
        if !self.playing {
            return self.floor;
        }
        let wall = self.floor + self.since.elapsed().as_secs_f32() * self.rate;
        let Some(a) = &self.audio else { return wall };
        let last = *a.shared.last.lock().unwrap();
        match last {
            // Use the sound card's position when it has reported since the last seek
            // and agrees with the system clock (a stalled or runaway device doesn't).
            Some((pos, at, latency)) if at >= self.since => {
                let heard = (pos as f64 / FRAC) / a.sample_rate as f64
                    + (at.elapsed().as_secs_f64() - latency.as_secs_f64()) * self.rate as f64;
                let heard = (heard as f32).max(self.floor);
                if (heard - wall).abs() < 0.25 {
                    heard
                } else {
                    wall
                }
            }
            _ => wall,
        }
    }

    /// Whether the last reading came from the sound card (for display).
    pub fn audio_locked(&self) -> bool {
        let Some(a) = &self.audio else { return false };
        self.playing
            && a.shared.last.lock().unwrap().is_some_and(|(_, at, _)| {
                at >= self.since && at.elapsed() < Duration::from_millis(250)
            })
    }

    pub fn play(&mut self) {
        if self.playing {
            return;
        }
        self.start_at(self.floor);
        self.playing = true;
        if let Some(a) = &self.audio {
            a.shared.playing.store(true, Ordering::Release);
        }
    }

    pub fn pause(&mut self) {
        if !self.playing {
            return;
        }
        let t = self.time();
        self.playing = false;
        if let Some(a) = &self.audio {
            a.shared.playing.store(false, Ordering::Release);
        }
        self.floor = t;
    }

    pub fn toggle(&mut self) {
        if self.playing {
            self.pause()
        } else {
            self.play()
        }
    }

    pub fn seek(&mut self, t: f32) {
        let t = t.max(0.0);
        if self.playing {
            self.start_at(t);
        } else {
            self.floor = t;
        }
    }

    fn start_at(&mut self, t: f32) {
        self.floor = t;
        self.since = Instant::now();
        if let Some(a) = &self.audio {
            *a.shared.last.lock().unwrap() = None;
            a.shared.pos.store(
                (t as f64 * a.sample_rate as f64 * FRAC) as u64,
                Ordering::Release,
            );
        }
    }

    pub fn set_rate(&mut self, rate: f32) {
        let t = self.time();
        self.rate = rate;
        if let Some(a) = &self.audio {
            a.shared.rate.store(rate.to_bits(), Ordering::Relaxed);
        }
        if self.playing {
            self.start_at(t);
        }
    }

    pub fn set_gain(&self, gain: f32) {
        if let Some(a) = &self.audio {
            a.shared.gain.store(gain.to_bits(), Ordering::Relaxed);
        }
    }
}

/// Write interleaved stereo f32 as a 16-bit WAV file.
pub fn write_wav(path: &std::path::Path, samples: &[f32], sample_rate: u32) -> std::io::Result<()> {
    use std::io::Write;
    let data_len = (samples.len() * 2) as u32;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_len).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&2u16.to_le_bytes())?; // stereo
    f.write_all(&sample_rate.to_le_bytes())?;
    f.write_all(&(sample_rate * 4).to_le_bytes())?;
    f.write_all(&4u16.to_le_bytes())?;
    f.write_all(&16u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data_len.to_le_bytes())?;
    for s in samples {
        f.write_all(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())?;
    }
    f.flush()
}
