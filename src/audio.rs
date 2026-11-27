use std::io::Cursor;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const SAMPLE_RATE: u32 = 16_000;

#[derive(Clone, Debug)]
pub struct RecordedAudio {
    pub sample_count: usize,
    pub duration_ms: u64,
    pub wav_bytes: Vec<u8>,
    /// The transcription that ran while recording, if one did (`live_transcribe`).
    pub live: crate::live_transcribe::Slot,
    /// Whether the microphone gave anything (`Sound::of`).
    pub sound: Sound,
    /// "used MacBook Pro Microphone from 0:42": a backup input stood in (`backupMics`, `merge`).
    pub switched: Option<String>,
    /// Each input's name and samples, lined up on this recording's clock (`backupMics`), so the
    /// bundle can keep them to choose from (`tracks`).
    pub tracks: Vec<(String, Vec<f32>)>,
}

/// What a capture's microphone gave, so a muted, wrong or broken input is noticed when the
/// capture finishes rather than when its transcript turns up empty.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sound {
    #[default]
    Heard,
    /// No audio, or none louder than `SILENT_DB` all the way through.
    Nothing,
    /// Sound until this many ms in, then the input gave out: it reported an error, or it sent
    /// only digital silence for the last `GAVE_OUT_MS` or more.
    StoppedAt(u64),
}

/// 50 ms windows, as the level is measured.
const WINDOW: usize = SAMPLE_RATE as usize / 20;
/// Quieter than any room heard through a working microphone: its own hiss is louder.
const SILENT_DB: f32 = -70.0;
/// A 16-bit sample of 0 or ±1: nothing is coming from the input at all.
const DEAD: f32 = 1.5 / 32_768.0;
const GAVE_OUT_MS: u64 = 3_000;

impl Sound {
    /// `broke_off`: the input reported an error partway (`AudioRecorder::stop`).
    pub fn of(samples: &[f32], broke_off: bool) -> Self {
        let heard = |w: &[f32]| {
            let rms = (w.iter().map(|x| x * x).sum::<f32>() / w.len() as f32).sqrt();
            20.0 * rms.max(1e-9).log10() >= SILENT_DB
        };
        let Some(last) = samples.chunks(WINDOW).rposition(heard) else {
            return Self::Nothing;
        };
        let end = ((last + 1) * WINDOW).min(samples.len());
        let ms = |n: usize| n as u64 * 1_000 / u64::from(SAMPLE_RATE);
        let tail = &samples[end..];
        if ms(tail.len()) >= GAVE_OUT_MS && tail.iter().all(|x| x.abs() < DEAD) {
            Self::StoppedAt(ms(end))
        } else if broke_off {
            Self::StoppedAt(ms(samples.len()))
        } else {
            Self::Heard
        }
    }

    /// For the capsule when the capture is done: `None` when there was sound.
    pub fn warning(self) -> Option<String> {
        match self {
            Self::Heard => None,
            Self::Nothing => Some("no sound".into()),
            Self::StoppedAt(ms) => Some(format!("no sound after {}", clock(ms))),
        }
    }
}

fn clock(ms: u64) -> String {
    format!("{}:{:02}", ms / 60_000, ms / 1_000 % 60)
}

pub struct AudioRecorder {
    worker: Option<std::thread::JoinHandle<Result<()>>>,
    stop_sender: Option<std::sync::mpsc::Sender<()>>,
    samples: Arc<Mutex<Vec<f32>>>,
    error: Arc<Mutex<Option<String>>>,
    /// When the first input's first samples came, to line the backups up with it.
    first: Arc<OnceLock<Instant>>,
    /// The first input's name, and the backups recording beside it (`backupMics`).
    name: Arc<Mutex<String>>,
    backups: Arc<Mutex<Vec<Backup>>>,
    live: Option<crate::live_transcribe::Live>,
    /// A live transcription only for showing the words (`live_audio`), dropped at stop.
    preview: Option<crate::live_transcribe::Live>,
    /// The capsule's view of this recording (`live_audio::begin`): the feed's id, which
    /// `follow_the_input_that_hears` moves to a backup while the first sends only silence.
    shown: Option<Arc<Mutex<u64>>>,
    /// Stops that watch.
    following: Option<Arc<std::sync::atomic::AtomicBool>>,
}

/// An input recording beside the first, in case that one hears nothing (`backupMics`).
struct Backup {
    name: String,
    samples: Arc<Mutex<Vec<f32>>>,
    error: Arc<Mutex<Option<String>>>,
    first: Arc<OnceLock<Instant>>,
}

impl AudioRecorder {
    pub fn start() -> Result<Self> {
        Self::open(true)
    }

    /// The microphone only for the capsule's waveform and words, while something else records the
    /// narration (the iPhone mirror records it into its movie). `None` when live audio is off.
    pub fn listen() -> Option<Self> {
        if !crate::live_audio::wanted() {
            return None;
        }
        Self::open(false)
            .inspect_err(|error| crate::capture::scrawl_log(format!("Live audio: {error:#}")))
            .ok()
    }

    /// `recording`: this is the capture's own audio, so the live transcription experiment runs on it.
    fn open(recording: bool) -> Result<Self> {
        let started = std::time::Instant::now();
        let samples = Arc::new(Mutex::new(Vec::new()));
        let error = Arc::new(Mutex::new(None));
        let first = Arc::new(OnceLock::new());
        let name = Arc::new(Mutex::new(String::new()));
        let backups = Arc::new(Mutex::new(Vec::new()));
        let (stop_sender, stop_receiver) = std::sync::mpsc::channel();
        let worker_samples = Arc::clone(&samples);
        let worker_error = Arc::clone(&error);
        let worker_first = Arc::clone(&first);
        let worker_name = Arc::clone(&name);
        let worker_backups = Arc::clone(&backups);
        let backup_mics = recording && crate::handoff::experiment("backupMics");
        let worker = std::thread::Builder::new()
            .name("scrawl-audio".into())
            .spawn(move || {
                let first = Track {
                    samples: worker_samples,
                    error: worker_error,
                    first: worker_first,
                };
                let stream = if backup_mics {
                    Self::open_streams(first, &worker_name, &worker_backups)
                } else {
                    Self::open_stream(first).map(|stream| vec![stream])
                }
                .inspect_err(|error| {
                    crate::capture::scrawl_log(format!("Microphone startup failed: {error:#}"));
                })?;
                if recording {
                    crate::ui::start_metric("audio_ready_ms", started);
                }
                // Stream never crosses threads. Disconnect also stops abandoned recorders.
                let _ = stop_receiver.recv();
                drop(stream);
                Ok(())
            })
            .context("Could not spawn microphone worker")?;
        let show = crate::live_audio::wanted();
        let caption = (show && crate::live_audio::captions_possible())
            .then(|| Arc::new(Mutex::new(crate::live_audio::Caption::default())));
        let log = |error: anyhow::Error| {
            crate::capture::scrawl_log(format!("Live transcription: {error:#}"));
        };
        // The experiment's stream shows the words too; without it, one only for showing them.
        let (live, preview) = if recording && crate::live_transcribe::wanted() {
            let live = crate::live_transcribe::Live::start(Arc::clone(&samples), caption.clone());
            (live.map_err(log).ok(), None)
        } else {
            let preview = caption.clone().map(|caption| {
                crate::live_transcribe::Live::preview(Arc::clone(&samples), caption)
            });
            (None, preview.and_then(|p| p.map_err(log).ok()))
        };
        let shown = show.then(|| {
            Arc::new(Mutex::new(crate::live_audio::begin(
                Arc::clone(&samples),
                caption.clone(),
            )))
        });
        let following = shown.as_ref().filter(|_| backup_mics).map(|shown| {
            follow_the_input_that_hears(
                Arc::clone(&samples),
                Arc::clone(&backups),
                caption,
                Arc::clone(shown),
            )
        });
        Ok(Self {
            worker: Some(worker),
            stop_sender: Some(stop_sender),
            samples,
            error,
            first,
            name,
            backups,
            live,
            preview,
            shown,
            following,
        })
    }

    /// Every input `mic::recording_order` names, at once: the first that opens fills `first`,
    /// the others record as backups. Falls back to `open_stream` when none opens.
    fn open_streams(
        first: Track,
        name: &Mutex<String>,
        backups: &Mutex<Vec<Backup>>,
    ) -> Result<Vec<cpal::Stream>> {
        let inputs = crate::mic::inputs();
        let order = crate::mic::recording_order(
            crate::mic::order().as_deref(),
            crate::mic::choice().as_ref(),
            &inputs,
        );
        let mut streams = Vec::new();
        for input in &order {
            let Some(device) = crate::mic::device(&input.id) else {
                continue;
            };
            let track = if streams.is_empty() {
                first.clone()
            } else {
                Track::default()
            };
            match Self::open_device(&device, track.clone()) {
                Ok(stream) => {
                    crate::capture::scrawl_log(format!(
                        "Microphone {} of {}: {}",
                        streams.len() + 1,
                        order.len(),
                        input.name
                    ));
                    if streams.is_empty() {
                        *name
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            input.name.clone();
                    } else {
                        backups
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(Backup {
                                name: input.name.clone(),
                                samples: track.samples,
                                error: track.error,
                                first: track.first,
                            });
                    }
                    streams.push(stream);
                }
                Err(e) => crate::capture::scrawl_log(format!(
                    "Microphone: could not open {} ({e:#})",
                    input.name
                )),
            }
        }
        if streams.is_empty() {
            streams.push(Self::open_stream(first)?);
        }
        Ok(streams)
    }

    /// The chosen microphone (`mic`), else the macOS default. A chosen device that is missing or
    /// won't open never stops the recording.
    fn open_stream(track: Track) -> Result<cpal::Stream> {
        let choice = crate::mic::choice();
        if let Some(choice) = &choice {
            if let Some(device) = crate::mic::device(&choice.id) {
                let input = crate::mic::Input {
                    id: choice.id.clone(),
                    name: device_name(&device).unwrap_or_else(|| choice.name.clone()),
                };
                crate::mic::note_pick(&crate::mic::Pick::Chosen(&input));
                match Self::open_device(&device, track.clone()) {
                    Ok(stream) => {
                        crate::capture::scrawl_log(format!("Microphone: {}", input.name));
                        return Ok(stream);
                    }
                    Err(e) => crate::capture::scrawl_log(format!(
                        "Microphone: could not open {} ({e:#}); using the default",
                        input.name
                    )),
                }
            } else {
                let default = cpal::default_host()
                    .default_input_device()
                    .and_then(|device| {
                        Some(crate::mic::Input {
                            id: device.id().ok()?.id().to_owned(),
                            name: device_name(&device)?,
                        })
                    });
                let inputs = crate::mic::Inputs {
                    default: default.as_ref().map(|d| d.id.clone()),
                    devices: default.into_iter().collect(),
                    built_in: None,
                };
                crate::mic::note_pick(&crate::mic::Pick::new(Some(choice), &inputs));
            }
        }
        let device = cpal::default_host()
            .default_input_device()
            .context("No microphone is available")?;
        let stream = Self::open_device(&device, track)?;
        crate::capture::scrawl_log(format!(
            "Microphone: {} (default)",
            device_name(&device).unwrap_or_default()
        ));
        Ok(stream)
    }

    fn open_device(device: &cpal::Device, track: Track) -> Result<cpal::Stream> {
        let supported = device.default_input_config()?;
        let config = supported.config();
        crate::capture::scrawl_log(format!(
            "Microphone: {} Hz, {} ch, {:?}",
            config.sample_rate,
            config.channels,
            supported.sample_format()
        ));
        ensure!(
            config.channels > 0 && config.sample_rate > 0,
            "Invalid microphone format"
        );
        macro_rules! stream {
            ($ty:ty) => {
                input_stream::<$ty>(&device, config, track.clone())?
            };
        }
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => stream!(f32),
            cpal::SampleFormat::F64 => stream!(f64),
            cpal::SampleFormat::I8 => stream!(i8),
            cpal::SampleFormat::I16 => stream!(i16),
            cpal::SampleFormat::I24 => stream!(cpal::I24),
            cpal::SampleFormat::I32 => stream!(i32),
            cpal::SampleFormat::I64 => stream!(i64),
            cpal::SampleFormat::U8 => stream!(u8),
            cpal::SampleFormat::U16 => stream!(u16),
            cpal::SampleFormat::U24 => stream!(cpal::U24),
            cpal::SampleFormat::U32 => stream!(u32),
            cpal::SampleFormat::U64 => stream!(u64),
            format => bail!("Unsupported microphone format: {format}"),
        };
        stream.play().context("Could not start microphone")?;
        Ok(stream)
    }

    fn join_worker(&mut self) -> Result<()> {
        self.stop_sender.take();
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow!("Microphone worker panicked"))??;
        }
        Ok(())
    }

    /// When the first input's first samples came, so another clock can line them up (`phone`).
    pub fn began(&self) -> Option<Instant> {
        self.first.get().copied()
    }

    pub fn stop(mut self) -> Result<RecordedAudio> {
        self.stop_showing();
        // Joining drops the stream and stops its callback before taking the buffer,
        // including a stop requested while the device was still being initialized.
        self.join_worker()?;
        let error = self
            .error
            .lock()
            .map_err(|_| anyhow!("Microphone error lock poisoned"))?
            .take();
        let samples_16k = std::mem::take(
            &mut *self
                .samples
                .lock()
                .map_err(|_| anyhow!("Microphone buffer lock poisoned"))?,
        );
        let (samples_16k, switched, backups_broke, tracks) = self.merge_backups(samples_16k);
        if switched.is_some() {
            // The words heard live came from the first input alone.
            self.live.take();
        }
        // An overload, or a device unplugged partway, keeps what was heard before it.
        if let Some(error) = &error {
            if samples_16k.is_empty() {
                bail!("Microphone capture failed: {error}");
            }
            crate::capture::scrawl_log(format!("Microphone: {error}; keeping what it heard"));
        }
        let sound = Sound::of(&samples_16k, error.is_some() && backups_broke);
        let sample_count = samples_16k.len();
        let duration_ms = sample_count as u64 * 1_000 / SAMPLE_RATE as u64;
        let wav_bytes = encode_wav_16k(&samples_16k)?;
        Ok(RecordedAudio {
            sample_count,
            duration_ms,
            wav_bytes,
            live: crate::live_transcribe::Slot::new(self.live.take()),
            sound,
            switched,
            tracks,
        })
    }

    /// The first input's samples merged with the backups' (`merge`), with what stood in, whether
    /// every backup broke off too (true without backups), and each input's own track (none
    /// without `backupMics`).
    #[allow(clippy::type_complexity)]
    fn merge_backups(
        &self,
        first: Vec<f32>,
    ) -> (Vec<f32>, Option<String>, bool, Vec<(String, Vec<f32>)>) {
        let backups = std::mem::take(
            &mut *self
                .backups
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if backups.is_empty() {
            let name = self
                .name
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let tracks = if name.is_empty() {
                Vec::new()
            } else {
                vec![(name, first.clone())]
            };
            return (first, None, true, tracks);
        }
        let start = self.first.get().copied();
        let mut names = vec![
            self.name
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        ];
        let mut takes = vec![first];
        let mut all_broke = true;
        for backup in backups {
            let mut samples = std::mem::take(
                &mut *backup
                    .samples
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            if let Some(error) = backup
                .error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                crate::capture::scrawl_log(format!("Microphone {}: {error}", backup.name));
            } else {
                all_broke = false;
            }
            // Lined up with the first input by when each one's samples began.
            let late_ms = match (start, backup.first.get()) {
                (Some(a), Some(&b)) if b >= a => b.duration_since(a).as_secs_f64() * 1e3,
                (Some(a), Some(&b)) => -a.duration_since(b).as_secs_f64() * 1e3,
                _ => 0.0,
            };
            align(
                &mut samples,
                (late_ms * f64::from(SAMPLE_RATE) / 1e3) as i64,
            );
            takes.push(samples);
            names.push(backup.name);
        }
        let (merged, chosen) = merge(&takes);
        let tracks: Vec<(String, Vec<f32>)> = names.iter().cloned().zip(takes).collect();
        let switched = chosen.iter().position(|&t| t != 0).map(|seg| {
            let name = &names[chosen[seg]];
            format!("used {name} from {}", clock(seg as u64 * 1_000))
        });
        crate::capture::scrawl_log(format!(
            "Microphones: {} · {}",
            names.join(", "),
            switched.as_deref().unwrap_or("the first heard it all")
        ));
        (merged, switched, all_broke, tracks)
    }
}

/// An input whose samples were all exactly zero (or that sent none) for this long is dead to the
/// capsule: AirPods the iPhone took, a muted interface.
const DEAD_AFTER: Duration = Duration::from_millis(1_500);

/// Which of `heard` (when each input last sent a sample other than zero) the capsule shows: the
/// first in the order that's alive, else the one it shows now.
fn input_to_show(heard: &[Option<Instant>], now: Instant, showing: usize) -> usize {
    heard
        .iter()
        .position(|at| at.is_some_and(|at| now.saturating_duration_since(at) < DEAD_AFTER))
        .unwrap_or(showing)
}

/// While recording with backups (`backupMics`), keeps the capsule's waveform on the first input
/// that's sending sound, so AirPods taken by the iPhone don't look like nothing is recording when
/// the Mac's microphone still is. Returns the flag that stops it.
fn follow_the_input_that_hears(
    first: Arc<Mutex<Vec<f32>>>,
    backups: Arc<Mutex<Vec<Backup>>>,
    caption: Option<Arc<Mutex<crate::live_audio::Caption>>>,
    shown: Arc<Mutex<u64>>,
) -> Arc<std::sync::atomic::AtomicBool> {
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = Arc::clone(&done);
    let _ = std::thread::Builder::new()
        .name("scrawl-audio-follow".into())
        .spawn(move || {
            let started = Instant::now();
            let mut read: Vec<usize> = Vec::new();
            let mut heard: Vec<Option<Instant>> = Vec::new();
            let mut showing = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
                let now = Instant::now();
                let mut inputs = vec![Arc::clone(&first)];
                inputs.extend(
                    backups
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .iter()
                        .map(|backup| Arc::clone(&backup.samples)),
                );
                read.resize(inputs.len(), 0);
                heard.resize(inputs.len(), Some(started));
                for (i, input) in inputs.iter().enumerate() {
                    let samples = input
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if samples
                        .get(read[i]..)
                        .is_some_and(|fresh| fresh.iter().any(|s| *s != 0.0))
                    {
                        heard[i] = Some(now);
                    }
                    read[i] = samples.len();
                }
                let next = input_to_show(&heard, now, showing);
                if next != showing {
                    let mut id = shown
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    crate::live_audio::end(*id);
                    *id = crate::live_audio::begin(Arc::clone(&inputs[next]), caption.clone());
                    crate::capture::scrawl_log(format!(
                        "Live audio: showing input {} of {}",
                        next + 1,
                        inputs.len()
                    ));
                    showing = next;
                }
            }
        });
    done
}

/// The input samples a stream fills, with its error and when its first samples came.
#[derive(Clone, Default)]
struct Track {
    samples: Arc<Mutex<Vec<f32>>>,
    error: Arc<Mutex<Option<String>>>,
    first: Arc<OnceLock<Instant>>,
}

/// Shifts `samples` later by `late` samples (silence before), or earlier when negative.
pub(crate) fn align(samples: &mut Vec<f32>, late: i64) {
    if late > 0 {
        samples.splice(0..0, std::iter::repeat_n(0.0, late as usize));
    } else {
        samples.drain(..(late.unsigned_abs() as usize).min(samples.len()));
    }
}

/// A second of each input is compared when merging.
const SEGMENT: usize = SAMPLE_RATE as usize;
/// A 50 ms window this much louder than its input's quiet (and louder than `SPEECH_MIN_DB`) is
/// someone speaking into it.
const SPEECH_OVER_QUIET_DB: f32 = 15.0;
const SPEECH_MIN_DB: f32 = -60.0;
/// Windows of speech it takes for an input to have heard a second.
const SPEECH_WINDOWS: usize = 3;
/// A switch between inputs fades across 10 ms.
const FADE: usize = SAMPLE_RATE as usize / 100;

/// One track from inputs recorded at once (`backupMics`), lined up and the preferred first: each
/// second comes from the first input that hears speech in it, so the preferred one whenever it
/// hears you. A second nobody speaks in stays with the input before it while that one still has
/// sound. Returns the track and which input each second came from.
pub fn merge(takes: &[Vec<f32>]) -> (Vec<f32>, Vec<usize>) {
    let len = takes.iter().map(Vec::len).max().unwrap_or(0);
    // Each input's level per window; `None` where it sent nothing (or digital silence).
    let levels: Vec<Vec<Option<f32>>> = takes
        .iter()
        .map(|take| {
            take.chunks(WINDOW)
                .map(|w| {
                    if w.iter().all(|x| x.abs() < DEAD) {
                        return None;
                    }
                    let rms = (w.iter().map(|x| x * x).sum::<f32>() / w.len() as f32).sqrt();
                    Some(20.0 * rms.max(1e-9).log10())
                })
                .collect()
        })
        .collect();
    // Its quiet: the level a fifth of its windows are under.
    let quiet: Vec<Option<f32>> = levels
        .iter()
        .map(|levels| {
            let mut heard: Vec<f32> = levels.iter().flatten().copied().collect();
            heard.sort_by(f32::total_cmp);
            heard.get(heard.len() / 5).copied()
        })
        .collect();
    let per_segment = SEGMENT / WINDOW;
    let mut chosen: Vec<usize> = Vec::new();
    let mut current = 0;
    for segment in 0..len.div_ceil(SEGMENT) {
        let windows = |t: usize| {
            let all = &levels[t];
            let start = (segment * per_segment).min(all.len());
            &all[start..((segment + 1) * per_segment).min(all.len())]
        };
        let hears = |t: usize| {
            quiet[t].is_some_and(|quiet| {
                windows(t)
                    .iter()
                    .flatten()
                    .filter(|&&db| db >= quiet + SPEECH_OVER_QUIET_DB && db >= SPEECH_MIN_DB)
                    .count()
                    >= SPEECH_WINDOWS
            })
        };
        let sounds = |t: usize| windows(t).iter().any(Option::is_some);
        current = (0..takes.len())
            .find(|&t| hears(t))
            .or_else(|| sounds(current).then_some(current))
            .or_else(|| (0..takes.len()).find(|&t| sounds(t)))
            .unwrap_or(current);
        chosen.push(current);
    }
    let sample = |t: usize, i: usize| takes[t].get(i).copied().unwrap_or(0.0);
    let mut merged = vec![0.0; len];
    for (segment, &t) in chosen.iter().enumerate() {
        let start = segment * SEGMENT;
        let end = (start + SEGMENT).min(len);
        for (i, out) in merged[start..end].iter_mut().enumerate() {
            *out = sample(t, start + i);
        }
        if let Some(&before) = segment.checked_sub(1).and_then(|s| chosen.get(s))
            && before != t
        {
            for k in 0..FADE.min(end - start) {
                let w = k as f32 / FADE as f32;
                merged[start + k] =
                    sample(before, start + k) * (1.0 - w) + sample(t, start + k) * w;
            }
        }
    }
    (merged, chosen)
}

impl AudioRecorder {
    /// Takes the recording out of the capsule and stops the words' own transcription.
    fn stop_showing(&mut self) {
        if let Some(following) = self.following.take() {
            following.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(id) = self.shown.take() {
            crate::live_audio::end(*id.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        }
        self.preview.take();
    }
}

impl Drop for AudioRecorder {
    fn drop(&mut self) {
        self.stop_showing();
        let _ = self.join_worker();
    }
}

fn device_name(device: &cpal::Device) -> Option<String> {
    Some(device.description().ok()?.name().to_owned())
}

fn input_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    track: Track,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let Track {
        samples,
        error,
        first,
    } = track;
    let channels = usize::from(config.channels);
    let rate = config.sample_rate;
    let mut phase = 0_u32;
    let mut sum = 0.0_f64;
    Ok(device.build_input_stream::<T, _, _>(
        config,
        move |data, _| {
            first.get_or_init(Instant::now);
            let Ok(mut output) = samples.lock() else {
                return;
            };
            for frame in data.chunks_exact(channels) {
                let mono = frame
                    .iter()
                    .map(|sample| sample.to_sample::<f32>() as f64)
                    .sum::<f64>()
                    / channels as f64;
                let mono = if mono.is_finite() { mono } else { 0.0 };
                // Area averaging gives a continuous-phase downmix/decimator,
                // including non-integer ratios such as 44.1 kHz -> 16 kHz.
                let mut remaining = SAMPLE_RATE;
                while remaining > 0 {
                    let weight = remaining.min(rate - phase);
                    sum += mono * weight as f64;
                    phase += weight;
                    remaining -= weight;
                    if phase == rate {
                        output.push((sum / rate as f64) as f32);
                        phase = 0;
                        sum = 0.0;
                    }
                }
            }
        },
        move |err| {
            // A buffer under- or overrun drops a few samples; the recording goes on.
            if err.kind() == cpal::ErrorKind::Xrun {
                return;
            }
            if let Ok(mut error) = error.lock() {
                *error = Some(err.to_string());
            }
        },
        None,
    )?)
}

/// Reads a 16 kHz mono WAV (as written by `scrawl-helper audio`) back into a recording.
pub fn recorded_audio_from_wav(wav_bytes: Vec<u8>) -> Result<RecordedAudio> {
    let sample_count = wav_16k_reader(&wav_bytes)?.duration() as usize;
    Ok(RecordedAudio {
        sample_count,
        duration_ms: sample_count as u64 * 1_000 / u64::from(SAMPLE_RATE),
        wav_bytes,
        live: Default::default(),
        switched: None,
        tracks: Vec::new(),
        sound: Sound::Heard,
    })
}

/// Decodes a 16 kHz mono 16-bit WAV (this module's own format) into samples.
pub fn decode_wav_16k(wav_bytes: &[u8]) -> Result<Vec<f32>> {
    wav_16k_reader(wav_bytes)?
        .into_samples::<i16>()
        .map(|sample| Ok(f32::from(sample?) / 32_768.0))
        .collect()
}

/// Decodes a 16 kHz mono 16-bit WAV into its raw samples.
pub fn decode_wav_pcm16(wav_bytes: &[u8]) -> Result<Vec<i16>> {
    Ok(wav_16k_reader(wav_bytes)?
        .into_samples::<i16>()
        .collect::<Result<_, _>>()?)
}

fn wav_16k_reader(wav_bytes: &[u8]) -> Result<hound::WavReader<Cursor<&[u8]>>> {
    let reader = hound::WavReader::new(Cursor::new(wav_bytes))?;
    let spec = reader.spec();
    ensure!(
        spec.sample_rate == SAMPLE_RATE
            && spec.channels == 1
            && spec.bits_per_sample == 16
            && spec.sample_format == hound::SampleFormat::Int,
        "Expected 16 kHz mono 16-bit audio"
    );
    Ok(reader)
}

const ISLAND_WINDOW_MS: u64 = 50;
const ISLAND_MERGE_SILENCE_MS: u64 = 350;

/// Energy-based speech spans in milliseconds, quantized to 50 ms. These are not recognizer
/// timestamps. The speech threshold follows the recording's own noise floor (10th percentile of
/// window energy) so a quiet microphone and a noisy room both find their speech.
pub fn speech_islands(samples_16k: &[f32]) -> Vec<(u64, u64)> {
    let window = (u64::from(SAMPLE_RATE) * ISLAND_WINDOW_MS / 1_000) as usize;
    let rms: Vec<f64> = samples_16k
        .chunks(window)
        .map(|chunk| {
            (chunk
                .iter()
                .map(|&x| {
                    if x.is_finite() {
                        f64::from(x).powi(2)
                    } else {
                        0.0
                    }
                })
                .sum::<f64>()
                / chunk.len() as f64)
                .sqrt()
        })
        .collect();
    if rms.is_empty() {
        return Vec::new();
    }
    let mut sorted = rms.clone();
    sorted.sort_by(f64::total_cmp);
    let floor = sorted[sorted.len() / 10];
    let threshold = (floor * 3.0).clamp(0.005, 0.02);
    let mut islands: Vec<(u64, u64)> = Vec::new();
    for (index, &level) in rms.iter().enumerate() {
        if level <= threshold {
            continue;
        }
        let start = index as u64 * ISLAND_WINDOW_MS;
        let end = start + ISLAND_WINDOW_MS;
        match islands.last_mut() {
            Some(last) if start.saturating_sub(last.1) < ISLAND_MERGE_SILENCE_MS => last.1 = end,
            _ => islands.push((start, end)),
        }
    }
    let duration = samples_16k.len() as u64 * 1_000 / u64::from(SAMPLE_RATE);
    for island in &mut islands {
        island.1 = island.1.min(duration);
    }
    islands.retain(|&(a, b)| b > a);
    islands
}

/// One sample as `audio.wav` stores it.
pub fn pcm16(sample: f32) -> i16 {
    let sample = if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    (sample * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16
}

pub fn encode_wav_16k(samples_16k: &[f32]) -> Result<Vec<u8>> {
    let mut bytes = Cursor::new(Vec::with_capacity(44 + samples_16k.len() * 2));
    {
        let mut writer = hound::WavWriter::new(
            &mut bytes,
            hound::WavSpec {
                channels: 1,
                sample_rate: SAMPLE_RATE,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )?;
        let mut i16_writer = writer.get_i16_writer(samples_16k.len() as u32);
        for &sample in samples_16k {
            i16_writer.write_sample(pcm16(sample));
        }
        i16_writer.flush()?;
        writer.finalize()?;
    }
    Ok(bytes.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn stop_joins_worker_before_reading_its_last_samples() {
        let samples = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::clone(&samples);
        let (stop_sender, stop_receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            // Model a final callback flushed while the worker drops its stream.
            assert!(matches!(
                stop_receiver.recv_timeout(Duration::from_secs(2)),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
            ));
            output.lock().unwrap().extend([0.1, 0.2, 0.3]);
            Ok(())
        });
        let audio = AudioRecorder {
            worker: Some(worker),
            stop_sender: Some(stop_sender),
            samples,
            error: Arc::new(Mutex::new(None)),
            first: Default::default(),
            name: Default::default(),
            backups: Default::default(),
            live: None,
            preview: None,
            shown: None,
            following: None,
        }
        .stop()
        .unwrap();
        assert_eq!(audio.sample_count, 3);
        assert_eq!(
            hound::WavReader::new(Cursor::new(audio.wav_bytes))
                .unwrap()
                .duration(),
            3
        );
    }

    #[test]
    fn sound_tells_a_dead_or_silent_input_from_a_quiet_room() {
        let second = SAMPLE_RATE as usize;
        // A working microphone in a quiet room: hiss around -60 dB, no speech.
        let hiss: Vec<f32> = (0..10 * second)
            .map(|i| if i % 2 == 0 { 0.001 } else { -0.001 })
            .collect();
        assert_eq!(Sound::of(&hiss, false), Sound::Heard);
        assert_eq!(Sound::of(&[], false), Sound::Nothing);
        assert_eq!(Sound::of(&vec![0.0; 10 * second], false), Sound::Nothing);
        assert_eq!(Sound::of(&vec![0.0001; 10 * second], false), Sound::Nothing);

        // Speech, then the input sends zeros: it gave out where the sound ended.
        let mut gave_out = hiss.clone();
        gave_out[4 * second..].fill(0.0);
        assert_eq!(Sound::of(&gave_out, false), Sound::StoppedAt(4_000));
        // A short gap of zeros at the end is a stop, not a dead input.
        let mut short = hiss.clone();
        short[9 * second..].fill(0.0);
        assert_eq!(Sound::of(&short, false), Sound::Heard);
        // An input that errored partway stopped where its samples end.
        assert_eq!(
            Sound::of(&hiss[..5 * second], true),
            Sound::StoppedAt(5_000)
        );

        assert_eq!(Sound::Heard.warning(), None);
        assert_eq!(Sound::Nothing.warning().as_deref(), Some("no sound"));
        assert_eq!(
            Sound::StoppedAt(62_500).warning().as_deref(),
            Some("no sound after 1:02")
        );
    }

    /// `seconds` of an input: quiet hiss at `quiet` (0 for none), with speech (a -20 dB tone)
    /// through the seconds `speaking` names.
    fn input(seconds: usize, quiet: f32, speaking: impl Fn(usize) -> bool) -> Vec<f32> {
        (0..seconds * SAMPLE_RATE as usize)
            .map(|i| {
                let hiss = if i % 2 == 0 { quiet } else { -quiet };
                if speaking(i / SAMPLE_RATE as usize) {
                    0.1 * (i as f32 * 0.2).sin()
                } else {
                    hiss
                }
            })
            .collect()
    }

    #[test]
    fn the_capsule_shows_the_first_input_still_sending_sound() {
        let now = Instant::now() + Duration::from_secs(10);
        let ago = |ms| Some(now - Duration::from_millis(ms));
        assert_eq!(input_to_show(&[ago(100), ago(100)], now, 0), 0);
        // AirPods taken by the iPhone: the first sends zeros, so the Mac's microphone shows.
        assert_eq!(input_to_show(&[ago(3_000), ago(100)], now, 0), 1);
        // Back as soon as the first sends sound again.
        assert_eq!(input_to_show(&[ago(100), ago(100)], now, 1), 0);
        // Nothing sending: it stays, and the red line says so.
        assert_eq!(input_to_show(&[ago(3_000), ago(3_000)], now, 1), 1);
        assert_eq!(input_to_show(&[None, ago(100)], now, 0), 1);
    }

    #[test]
    fn backups_stand_in_only_where_the_first_input_hears_nothing() {
        let talk = |s: usize| s % 3 != 2;
        let airpods = input(9, 0.0005, talk);
        let mac = input(9, 0.003, talk);
        // The first hears it all: it alone, untouched.
        let (merged, chosen) = merge(&[airpods.clone(), mac.clone()]);
        assert_eq!(chosen, vec![0; 9]);
        assert_eq!(merged, airpods);

        // AirPods go to the iPhone 4 s in: zeros from then (or no samples at all).
        let mut gone = airpods.clone();
        gone[4 * SAMPLE_RATE as usize..].fill(0.0);
        let (merged, chosen) = merge(&[gone.clone(), mac.clone()]);
        assert_eq!(chosen, [0, 0, 0, 0, 1, 1, 1, 1, 1]);
        assert_eq!(merged.len(), mac.len());
        assert_eq!(
            merged[6 * SAMPLE_RATE as usize],
            mac[6 * SAMPLE_RATE as usize]
        );
        let (_, chosen) = merge(&[airpods[..4 * SAMPLE_RATE as usize].to_vec(), mac.clone()]);
        assert_eq!(chosen, [0, 0, 0, 0, 1, 1, 1, 1, 1]);

        // A wrong input that only hisses: the backup, from the first word.
        let (_, chosen) = merge(&[input(9, 0.0005, |_| false), mac.clone()]);
        assert_eq!(chosen, [1; 9]);

        // Back from the backup as soon as the first hears speech again; a quiet second stays.
        let mut dropout = airpods.clone();
        dropout[3 * SAMPLE_RATE as usize..5 * SAMPLE_RATE as usize].fill(0.0);
        let (_, chosen) = merge(&[dropout, mac.clone()]);
        assert_eq!(chosen, [0, 0, 0, 1, 1, 1, 0, 0, 0]);

        // Nobody speaks: the first, all the way.
        let (_, chosen) = merge(&[input(4, 0.0005, |_| false), input(4, 0.003, |_| false)]);
        assert_eq!(chosen, [0; 4]);
    }

    #[test]
    fn a_backup_lines_up_with_the_first_input_by_when_it_began() {
        let mut late = vec![1.0, 2.0];
        align(&mut late, 2);
        assert_eq!(late, [0.0, 0.0, 1.0, 2.0]);
        let mut early = vec![1.0, 2.0, 3.0];
        align(&mut early, -2);
        assert_eq!(early, [3.0]);
        align(&mut early, -5);
        assert!(early.is_empty());
    }

    #[test]
    fn a_device_lost_partway_keeps_what_it_heard() {
        let recorder = |samples: Vec<f32>| AudioRecorder {
            worker: None,
            stop_sender: None,
            samples: Arc::new(Mutex::new(samples)),
            error: Arc::new(Mutex::new(Some("Device disconnected".into()))),
            first: Default::default(),
            name: Default::default(),
            backups: Default::default(),
            live: None,
            preview: None,
            shown: None,
            following: None,
        };
        assert_eq!(recorder(vec![0.1; 160]).stop().unwrap().sample_count, 160);
        assert!(recorder(Vec::new()).stop().is_err());
    }

    #[test]
    fn stop_propagates_background_initialization_failure() {
        let recorder = AudioRecorder {
            worker: Some(std::thread::spawn(|| bail!("device unavailable"))),
            stop_sender: None,
            samples: Arc::new(Mutex::new(Vec::new())),
            error: Arc::new(Mutex::new(None)),
            first: Default::default(),
            name: Default::default(),
            backups: Default::default(),
            live: None,
            preview: None,
            shown: None,
            following: None,
        };
        assert!(
            recorder
                .stop()
                .unwrap_err()
                .to_string()
                .contains("device unavailable")
        );
    }

    #[test]
    fn speech_islands_merge_only_short_silences() {
        let mut samples = vec![0.0; 32_000];
        samples[1_600..4_800].fill(0.2); // 100..300
        samples[8_800..12_800].fill(0.2); // 550..800: merge 250 ms gap
        samples[18_400..20_800].fill(0.2); // 1150..1300: keep 350 ms gap
        assert_eq!(speech_islands(&samples), vec![(100, 800), (1_150, 1_300)]);
        assert!(speech_islands(&[0.001; 800]).is_empty());
        assert!(speech_islands(&[]).is_empty());
    }

    #[test]
    fn speech_islands_adapt_to_a_noisy_floor() {
        let mut samples: Vec<f32> = (0..32_000)
            .map(|i| if i % 2 == 0 { 0.006 } else { -0.006 })
            .collect();
        samples[8_000..16_000]
            .iter_mut()
            .for_each(|s| *s = if *s > 0.0 { 0.15 } else { -0.15 });
        assert_eq!(speech_islands(&samples), vec![(500, 1_000)]);
    }

    #[test]
    fn decoded_wav_round_trips() {
        let wav = encode_wav_16k(&[0.0, 0.5, -0.5]).unwrap();
        assert_eq!(decode_wav_16k(&wav).unwrap(), vec![0.0, 0.5, -0.5]);
        assert!(decode_wav_16k(b"nope").is_err());
    }

    #[test]
    fn wav_is_mono_16k_pcm_and_clips_safely() {
        let bytes = encode_wav_16k(&[-2.0, -0.5, 0.0, 0.5, 2.0, f32::NAN]).unwrap();
        let mut reader = hound::WavReader::new(Cursor::new(bytes)).unwrap();
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(reader.spec().sample_rate, 16_000);
        assert_eq!(reader.spec().bits_per_sample, 16);
        assert_eq!(reader.spec().sample_format, hound::SampleFormat::Int);
        assert_eq!(
            reader
                .samples::<i16>()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap(),
            vec![-32_768, -16_384, 0, 16_384, 32_767, 0]
        );
    }
}
