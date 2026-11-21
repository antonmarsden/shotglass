//! Audio output and the master clock.
//!
//! While playing, *the sound card decides what time it is*: the picture asks the
//! clock for the time of the sample being heard right now, so a slow frame becomes a
//! skipped frame instead of a slow-motion drift out of sync. Without an audio device
//! the clock falls back to the system timer.

use arc_swap::ArcSwap;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FRAC: f64 = 4294967296.0; // 32.32 fixed point sample position

/// Synthesised audio the callback can play: chunks of interleaved stereo, each
/// tagged with the soundtrack version it came from.
#[derive(Clone, Default)]
pub struct ChunkStore {
    pub chunk: usize,
    pub total: usize,
    pub chunks: HashMap<usize, (u64, Arc<Vec<f32>>)>,
    /// A loop film: the play position wraps, sample `i` is sample `i mod total`.
    pub wrap: bool,
}

/// Where a looping play position wraps (samples): a whole number of periods of
/// `total` samples, at least 2^28 (≈ 1.5 h at 48 kHz), so the 32.32 position never
/// overflows and the clock can tell which period the sound card is in by comparing
/// with the system timer (they agree within 0.25 s).
pub fn wrap_span(total: usize) -> u64 {
    let total = total.max(1) as u64;
    total * (1u64 << 28).div_ceil(total)
}

struct Shared {
    store: ArcSwap<ChunkStore>,
    /// Samples the callback had to play as silence because their chunk wasn't ready.
    underruns: AtomicU64,
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
    _stream: Option<cpal::Stream>,
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
            store: ArcSwap::from_pointee(ChunkStore::default()),
            underruns: AtomicU64::new(0),
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
            _stream: Some(stream),
        })
    }

    /// An output with no real device, driven by calling `FakeDevice::callback` (tests).
    #[cfg(test)]
    pub fn fake(sample_rate: u32) -> (AudioOut, FakeDevice) {
        let shared = Arc::new(Shared {
            store: ArcSwap::from_pointee(ChunkStore::default()),
            underruns: AtomicU64::new(0),
            pos: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            rate: AtomicU32::new(1f32.to_bits()),
            gain: AtomicU32::new(1f32.to_bits()),
            last: Mutex::new(None),
        });
        let dev = FakeDevice {
            shared: shared.clone(),
        };
        (
            AudioOut {
                shared,
                sample_rate,
                device_name: "fake".into(),
                _stream: None,
            },
            dev,
        )
    }
}

/// Plays the role of the sound card's callback thread in tests.
#[cfg(test)]
#[derive(Clone)]
pub struct FakeDevice {
    shared: Arc<Shared>,
}

#[cfg(test)]
impl FakeDevice {
    /// Ask for `frames` stereo frames, as a sound card would, with this output latency.
    pub fn callback(&self, frames: usize, latency: Duration) -> Vec<f32> {
        let mut out = vec![0f32; frames * 2];
        fill(&self.shared, &mut out, 2, latency);
        out
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
            fill(&sh, data, channels, ts.playback - ts.callback);
        },
        |e| eprintln!("audio: {e}"),
        None,
    )?;
    Ok(stream)
}

/// The audio callback: copy the chunks under the play position into `data` and
/// advance it. Lock-free apart from a `try_lock` that never waits.
fn fill<T: SizedSample + FromSample<f32>>(
    sh: &Shared,
    data: &mut [T],
    channels: usize,
    latency: Duration,
) {
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
    let store = sh.store.load();
    let mut cur: Option<(usize, &Arc<Vec<f32>>)> = None;
    let mut missing = 0u64;
    let rate = f32::from_bits(sh.rate.load(Ordering::Relaxed)) as f64;
    let gain = f32::from_bits(sh.gain.load(Ordering::Relaxed));
    let step = (rate * FRAC) as u64;
    let wrap = (store.wrap && store.total > 0).then(|| wrap_span(store.total) << 32);
    for frame in data.chunks_mut(channels) {
        let mut i = (pos >> 32) as usize;
        if wrap.is_some() {
            i %= store.total;
        }
        let (l, r) = if store.chunk > 0 && i < store.total {
            let c = i / store.chunk;
            if cur.is_none_or(|(cc, _)| cc != c) {
                cur = store.chunks.get(&c).map(|(_, d)| (c, d));
            }
            match cur {
                Some((_, d)) => {
                    let j = (i - c * store.chunk) * 2;
                    if j + 1 < d.len() {
                        (d[j] * gain, d[j + 1] * gain)
                    } else {
                        (0.0, 0.0)
                    }
                }
                None => {
                    missing += 1;
                    (0.0, 0.0)
                }
            }
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
        if let Some(span) = wrap
            && pos >= span
        {
            pos %= span;
        }
    }
    if missing > 0 {
        sh.underruns.fetch_add(missing, Ordering::Relaxed);
    }
    // Only advance if nobody seeked meanwhile (a seek wins).
    let _ = sh
        .pos
        .compare_exchange(start, pos, Ordering::AcqRel, Ordering::Relaxed);
}

pub struct Clock {
    audio: Option<AudioOut>,
    playing: bool,
    rate: f32,
    /// Time while paused; the time at the last play/seek while playing. Seconds, f64
    /// (playback time isn't wrapped internally, so it must stay precise for hours).
    floor: f64,
    /// When the last play/seek happened.
    since: Instant,
    /// The system-timer estimate: (time, when). Moved to the sound card's reading
    /// whenever that is trusted, so the timer follows the card's slow drift (a loop
    /// can play for days) and stays a good fallback.
    base: std::cell::Cell<(f64, Instant)>,
    /// The last time handed out while playing: time never runs backwards, even when
    /// the source switches between the sound card and the system timer.
    reported: std::cell::Cell<f64>,
    /// A loop film: (period in seconds, where the sound card position wraps in
    /// samples). `time()` is then within one period.
    period: Option<(f64, u64)>,
}

impl Clock {
    pub fn new(audio: Option<AudioOut>) -> Clock {
        Clock {
            audio,
            playing: false,
            rate: 1.0,
            floor: 0.0,
            since: Instant::now(),
            base: std::cell::Cell::new((0.0, Instant::now())),
            reported: std::cell::Cell::new(0.0),
            period: None,
        }
    }

    /// Loop every `samples` samples (at `sample_rate()`), or don't (None). While
    /// looping, the sound card plays across the wrap and `time()` stays within the
    /// period; the playhead keeps its place within it.
    pub fn set_loop(&mut self, samples: Option<usize>) {
        let sr = self.sample_rate() as f64;
        let new = samples.map(|n| (n.max(1) as f64 / sr, wrap_span(n)));
        if let Some(a) = &self.audio {
            let mut st = (**a.shared.store.load()).clone();
            st.wrap = samples.is_some();
            if let Some(n) = samples
                && st.total != n
            {
                st.total = n;
                st.chunks.clear();
            }
            a.shared.store.store(Arc::new(st));
        }
        if new.map(|p| p.0) == self.period.map(|p| p.0) {
            return;
        }
        let t = self.time() as f64;
        self.period = new;
        if self.playing {
            self.start_at(t);
        } else {
            self.floor = self.wrap(t);
        }
    }

    pub fn looping(&self) -> bool {
        self.period.is_some()
    }

    /// The loop period in samples, if looping.
    pub fn loop_samples(&self) -> Option<usize> {
        self.period
            .map(|(p, _)| (p * self.sample_rate() as f64).round() as usize)
    }

    /// Whole periods played since the last play/seek (0 when not looping).
    pub fn loops(&self) -> u64 {
        match self.period {
            Some((p, _)) if self.playing => (self.reported.get() / p).floor().max(0.0) as u64,
            _ => 0,
        }
    }

    fn wrap(&self, t: f64) -> f64 {
        match self.period {
            Some((p, _)) => crate::movie::wrap_time(t, p),
            None => t,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.audio.as_ref().map_or(44100, |a| a.sample_rate)
    }

    /// Version of chunk `c` that the sound card can currently play, if any.
    pub fn chunk_version(&self, c: usize) -> Option<u64> {
        self.audio
            .as_ref()?
            .shared
            .store
            .load()
            .chunks
            .get(&c)
            .map(|(v, _)| *v)
    }

    /// Hand a synthesised chunk to the sound card.
    pub fn put_chunk(
        &self,
        chunk: usize,
        total: usize,
        c: usize,
        version: u64,
        data: Arc<Vec<f32>>,
    ) {
        let Some(a) = &self.audio else { return };
        let mut st = (**a.shared.store.load()).clone();
        if st.chunk != chunk || st.total != total {
            st.chunk = chunk;
            st.total = total;
            st.chunks.retain(|_, (v, _)| *v == version);
        }
        st.chunks.insert(c, (version, data));
        a.shared.store.store(Arc::new(st));
    }

    /// Forget chunks `keep` says no to, so memory stays bounded.
    pub fn drop_chunks_where(&self, keep: impl Fn(usize) -> bool) {
        let Some(a) = &self.audio else { return };
        let cur = a.shared.store.load();
        if cur.chunks.keys().all(|c| keep(*c)) {
            return;
        }
        let mut st = (**cur).clone();
        st.chunks.retain(|c, _| keep(*c));
        a.shared.store.store(Arc::new(st));
    }

    pub fn clear_chunks(&self) {
        if let Some(a) = &self.audio {
            let old = a.shared.store.load();
            a.shared.store.store(Arc::new(ChunkStore {
                wrap: old.wrap,
                total: if old.wrap { old.total } else { 0 },
                ..ChunkStore::default()
            }));
        }
    }

    pub fn underruns(&self) -> u64 {
        self.audio
            .as_ref()
            .map_or(0, |a| a.shared.underruns.load(Ordering::Relaxed))
    }

    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    pub fn playing(&self) -> bool {
        self.playing
    }

    pub fn rate(&self) -> f32 {
        self.rate
    }

    /// The movie time being *heard* right now. Never decreases while playing, except
    /// that a loop film's time wraps to 0 at the end of each period.
    pub fn time(&self) -> f32 {
        if !self.playing {
            return self.wrap(self.floor) as f32;
        }
        let t = self.source_time().max(self.reported.get());
        self.reported.set(t);
        self.wrap(t) as f32
    }

    /// Unwrapped playback time (seconds since the last play/seek's origin).
    fn source_time(&self) -> f64 {
        let (bt, bat) = self.base.get();
        let wall = bt + bat.elapsed().as_secs_f64() * self.rate as f64;
        let Some(a) = &self.audio else { return wall };
        let last = *a.shared.last.lock().unwrap();
        match last {
            // Use the sound card's position when it has reported since the last seek
            // and agrees with the system clock (a stalled or runaway device doesn't).
            Some((pos, at, latency)) if at >= self.since => {
                let sr = a.sample_rate as f64;
                let mut heard = (pos as f64 / FRAC) / sr
                    + (at.elapsed().as_secs_f64() - latency.as_secs_f64()) * self.rate as f64;
                // A looping position wraps; put it in the span the timer is in.
                if let Some((_, span)) = self.period {
                    let span = span as f64 / sr;
                    heard += ((wall - heard) / span).round() * span;
                }
                let heard = heard.max(self.floor);
                if (heard - wall).abs() < 0.25 {
                    self.base.set((heard, Instant::now()));
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
        let t = self.source_time().max(self.reported.get());
        self.playing = false;
        if let Some(a) = &self.audio {
            a.shared.playing.store(false, Ordering::Release);
        }
        self.floor = self.wrap(t);
    }

    pub fn toggle(&mut self) {
        if self.playing {
            self.pause()
        } else {
            self.play()
        }
    }

    pub fn seek(&mut self, t: f32) {
        let t = self.wrap(t.max(0.0) as f64);
        if self.playing {
            self.start_at(t);
        } else {
            self.floor = t;
        }
    }

    fn start_at(&mut self, t: f64) {
        let t = self.wrap(t);
        self.floor = t;
        self.since = Instant::now();
        self.base.set((t, self.since));
        self.reported.set(t);
        if let Some(a) = &self.audio {
            *a.shared.last.lock().unwrap() = None;
            a.shared
                .pos
                .store((t * a.sample_rate as f64 * FRAC) as u64, Ordering::Release);
        }
    }

    pub fn set_rate(&mut self, rate: f32) {
        let t = self.time() as f64;
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

/// Streams interleaved stereo f32 to a 16-bit WAV file.
pub struct WavWriter {
    f: std::io::BufWriter<std::fs::File>,
}

impl WavWriter {
    /// `frames` = total stereo frames that will be written.
    pub fn create(
        path: &std::path::Path,
        frames: usize,
        sample_rate: u32,
    ) -> std::io::Result<WavWriter> {
        use std::io::Write;
        let data_len = (frames * 4) as u32;
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
        Ok(WavWriter { f })
    }

    pub fn write(&mut self, samples: &[f32]) -> std::io::Result<()> {
        use std::io::Write;
        for s in samples {
            self.f
                .write_all(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> std::io::Result<()> {
        use std::io::Write;
        self.f.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    const SR: u32 = 48000;
    const LATENCY: Duration = Duration::from_millis(20);

    /// Drive a fake device like a real one: every few ms it asks for as many frames
    /// as have elapsed (times `speed`), until `stop` is set. Pacing by elapsed time
    /// keeps it real-time even where sleep is coarse (Windows: ~15 ms).
    fn run_device(
        dev: FakeDevice,
        speed: f64,
        stop: Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut last = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                sleep(Duration::from_millis(5));
                let now = Instant::now();
                let frames = ((now - last).as_secs_f64() * SR as f64 * speed).round() as usize;
                last = now;
                dev.callback(frames.max(1), LATENCY);
            }
        })
    }

    fn sample_for(clock: &Clock, dur: Duration) -> Vec<f32> {
        let end = Instant::now() + dur;
        let mut v = vec![];
        while Instant::now() < end {
            v.push(clock.time());
            sleep(Duration::from_millis(1));
        }
        v
    }

    fn monotonic(v: &[f32]) -> bool {
        v.windows(2).all(|w| w[1] >= w[0])
    }

    #[test]
    fn paused_clock_holds_and_seeks() {
        let mut c = Clock::new(None);
        assert_eq!(c.time(), 0.0);
        c.seek(3.5);
        sleep(Duration::from_millis(20));
        assert_eq!(c.time(), 3.5);
        c.seek(-1.0);
        assert_eq!(c.time(), 0.0);
    }

    #[test]
    fn no_device_follows_the_system_timer() {
        let mut c = Clock::new(None);
        let start = Instant::now();
        c.play();
        sleep(Duration::from_millis(100));
        let t = c.time();
        // A busy CI machine can oversleep a lot (macOS: 100 ms took 220): bound t by
        // the time that really passed, not the time asked for.
        let elapsed = start.elapsed().as_secs_f32();
        assert!(
            (0.09..=elapsed + 0.01).contains(&t),
            "{t} after {elapsed} s"
        );
        c.pause();
        let held = c.time();
        sleep(Duration::from_millis(30));
        assert_eq!(c.time(), held);
    }

    #[test]
    fn realistic_device_locks_and_never_runs_backwards() {
        let (out, dev) = AudioOut::fake(SR);
        let mut c = Clock::new(Some(out));
        let stop = Arc::new(AtomicBool::new(false));
        let h = run_device(dev, 1.0, stop.clone());
        c.seek(10.0);
        c.play();
        let v = sample_for(&c, Duration::from_millis(300));
        let locked = c.audio_locked();
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
        assert!(monotonic(&v), "time went backwards");
        assert!(locked, "never locked to the device");
        let last = *v.last().unwrap();
        // ≈ 0.3 s of playback, minus the 20 ms the sound card hasn't played yet
        assert!((10.2..10.36).contains(&last), "{last}");
        assert!(v[0] >= 10.0);
    }

    #[test]
    fn stalled_device_falls_back_to_the_timer() {
        let (out, dev) = AudioOut::fake(SR);
        let mut c = Clock::new(Some(out));
        c.play();
        dev.callback(480, LATENCY); // one callback, then silence
        let v = sample_for(&c, Duration::from_millis(400));
        assert!(monotonic(&v));
        let last = *v.last().unwrap();
        assert!((0.33..0.5).contains(&last), "{last}");
        assert!(!c.audio_locked());
    }

    #[test]
    fn runaway_device_is_ignored() {
        // Like ALSA's null device: consumes audio 100× faster than real time.
        let (out, dev) = AudioOut::fake(SR);
        let mut c = Clock::new(Some(out));
        let stop = Arc::new(AtomicBool::new(false));
        let h = run_device(dev, 100.0, stop.clone());
        c.play();
        let v = sample_for(&c, Duration::from_millis(200));
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
        assert!(monotonic(&v));
        let last = *v.last().unwrap();
        assert!((0.15..0.3).contains(&last), "{last}");
    }

    #[test]
    fn seeking_while_playing_lands_on_the_target() {
        let (out, dev) = AudioOut::fake(SR);
        let mut c = Clock::new(Some(out));
        let stop = Arc::new(AtomicBool::new(false));
        let h = run_device(dev, 1.0, stop.clone());
        c.play();
        sleep(Duration::from_millis(100));
        for target in [5.0, 2.0, 7.5] {
            c.seek(target);
            let t = c.time();
            assert!((target..target + 0.02).contains(&t), "seek {target}: {t}");
            let v = sample_for(&c, Duration::from_millis(80));
            assert!(monotonic(&v));
            assert!(
                v.iter().all(|x| *x >= target && *x < target + 0.2),
                "after seek {target}: {:?}",
                (v[0], v[v.len() - 1])
            );
        }
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
    }

    #[test]
    fn half_speed_runs_at_half_speed() {
        let mut c = Clock::new(None);
        c.set_rate(0.5);
        let start = Instant::now();
        c.play();
        sleep(Duration::from_millis(200));
        let t = c.time();
        let elapsed = start.elapsed().as_secs_f32();
        assert!(
            (0.09..=elapsed / 2.0 + 0.01).contains(&t),
            "{t} after {elapsed} s"
        );
    }

    #[test]
    fn callback_plays_chunks_and_counts_underruns() {
        let (out, dev) = AudioOut::fake(SR);
        let mut c = Clock::new(Some(out));
        // chunk 0 holds a ramp; chunk 1 is missing
        let ramp: Vec<f32> = (0..100)
            .flat_map(|i| [i as f32 / 100.0, -(i as f32) / 100.0])
            .collect();
        c.put_chunk(100, 200, 0, 1, Arc::new(ramp));
        c.play();
        let a = dev.callback(100, LATENCY);
        assert_eq!(a[2], 0.01);
        assert_eq!(a[3], -0.01);
        assert_eq!(c.underruns(), 0);
        let b = dev.callback(50, LATENCY);
        assert!(b.iter().all(|x| *x == 0.0));
        assert_eq!(c.underruns(), 50);
    }

    #[test]
    fn callback_plays_across_the_loop_seam() {
        let (out, dev) = AudioOut::fake(SR);
        let mut c = Clock::new(Some(out));
        // A 250-sample loop in chunks of 100 (the last one short): a ramp 0..250.
        let ramp = |a: usize, b: usize| -> Arc<Vec<f32>> {
            Arc::new((a..b).flat_map(|i| [i as f32, -(i as f32)]).collect())
        };
        c.set_loop(Some(250));
        for (i, (a, b)) in [(0, 100), (100, 200), (200, 250)].into_iter().enumerate() {
            c.put_chunk(100, 250, i, 1, ramp(a, b));
        }
        c.seek(240.5 / SR as f32);
        c.play();
        let v = dev.callback(530, LATENCY);
        let left: Vec<f32> = v.iter().step_by(2).copied().collect();
        // 240..249, then the loop again from 0, twice over: no gap, no silence.
        assert_eq!(left[0], 240.0);
        let want: Vec<f32> = (240..240 + 530).map(|i| (i % 250) as f32).collect();
        assert_eq!(left, want);
        assert_eq!(c.underruns(), 0);
        // Without the loop the same position plays past the end into silence.
        c.set_loop(None);
        c.seek(240.5 / SR as f32);
        c.play();
        let v = dev.callback(20, LATENCY);
        assert_eq!(v[2 * 9], 249.0);
        assert!(v[2 * 10..].iter().all(|x| *x == 0.0));
    }

    #[test]
    fn looping_position_wraps_instead_of_overflowing() {
        // A position near the 32.32 limit, as after a day of playback: with a loop it
        // stays within the wrap span (a whole number of periods).
        assert_eq!(wrap_span(250) % 250, 0);
        assert!(wrap_span(250) >= 1 << 28);
        assert_eq!(wrap_span(1 << 30), 1 << 30);
        let (out, dev) = AudioOut::fake(SR);
        let mut c = Clock::new(Some(out));
        c.set_loop(Some(250));
        c.put_chunk(
            250,
            250,
            0,
            1,
            Arc::new((0..250).flat_map(|i| [i as f32, 0.0]).collect()),
        );
        let span = wrap_span(250);
        c.play();
        let shared = &c.audio.as_ref().unwrap().shared;
        shared.pos.store((span - 3) << 32, Ordering::Release);
        let v = dev.callback(6, LATENCY);
        let left: Vec<f32> = v.iter().step_by(2).copied().collect();
        let want: Vec<f32> = (0..6).map(|k| ((span - 3 + k) % 250) as f32).collect();
        assert_eq!(left, want);
        let pos = c.audio.as_ref().unwrap().shared.pos.load(Ordering::Relaxed) >> 32;
        assert_eq!(pos, 3);
    }

    #[test]
    fn looping_clock_wraps_time_to_the_period() {
        let mut c = Clock::new(None);
        c.set_loop(Some(4410)); // 0.1 s at 44.1 kHz (no device)
        c.seek(0.25);
        assert!((c.time() - 0.05).abs() < 1e-6, "{}", c.time());
        c.seek(0.08);
        let start = Instant::now();
        c.play();
        let mut wrapped = false;
        let mut last = c.time();
        while start.elapsed() < Duration::from_millis(250) {
            let t = c.time();
            assert!((0.0..0.1).contains(&t), "{t}");
            if t < last {
                wrapped = true;
                // Wraps only at the end of a period.
                assert!(last > 0.07 && t < 0.03, "{last} -> {t}");
            }
            last = t;
            sleep(Duration::from_millis(1));
        }
        assert!(wrapped && c.loops() >= 1);
        c.pause();
        let held = c.time();
        assert!((0.0..0.1).contains(&held));
        // Leaving the loop keeps the place within the period.
        c.set_loop(None);
        assert_eq!(c.time(), held);
    }
}
