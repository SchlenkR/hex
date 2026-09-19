use std::io::Cursor;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{OnceLock, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use color_eyre::eyre::{Result, WrapErr, eyre};
use rodio::buffer::SamplesBuffer;
use rodio::{Decoder, DeviceSinkBuilder, Source};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tone {
    #[cfg(target_os = "macos")]
    Error,
    DictationStart,
    DictationStop,
    Cancel,
}

static DICTATION_PLAYER: OnceLock<SyncSender<Tone>> = OnceLock::new();
static LOADER_STARTED: AtomicBool = AtomicBool::new(false);
static ENABLED: AtomicBool = AtomicBool::new(true);
static VOLUME: AtomicU32 = AtomicU32::new(0.5_f32.to_bits());
static SOUND_SELECTION: OnceLock<RwLock<SoundSelection>> = OnceLock::new();
static SOUND_GENERATION: AtomicU64 = AtomicU64::new(0);

pub const DEFAULT_SOUND: &str = "default";
pub const SYSTEM_SOUNDS: &[&str] = &[
    "Basso",
    "Blow",
    "Bottle",
    "Frog",
    "Funk",
    "Glass",
    "Hero",
    "Morse",
    "Ping",
    "Pop",
    "Purr",
    "Sosumi",
    "Submarine",
    "Tink",
];

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SoundSelection {
    start: String,
    stop: String,
}

pub fn set_sounds(start: &str, stop: &str) {
    let selection = SoundSelection {
        start: start.to_owned(),
        stop: stop.to_owned(),
    };
    let mut current = SOUND_SELECTION
        .get_or_init(Default::default)
        .write()
        .unwrap_or_else(|error| error.into_inner());
    if *current != selection {
        *current = selection;
        SOUND_GENERATION.fetch_add(1, Ordering::Release);
    }
}

fn sound_selection() -> SoundSelection {
    SOUND_SELECTION
        .get_or_init(Default::default)
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

pub fn sound_label(value: &str) -> String {
    if value.is_empty() || value == DEFAULT_SOUND {
        "Hex".into()
    } else if SYSTEM_SOUNDS.contains(&value) {
        value.into()
    } else {
        Path::new(value)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| value.into())
    }
}

fn resolve_sound(value: &str, bundled: &SamplesBuffer) -> SamplesBuffer {
    if value.is_empty() || value == DEFAULT_SOUND {
        return bundled.clone();
    }
    let path = if SYSTEM_SOUNDS.contains(&value) {
        format!("/System/Library/Sounds/{value}.aiff")
    } else {
        value.to_owned()
    };
    match std::fs::read(&path)
        .map_err(Into::into)
        .and_then(decode_bytes)
    {
        Ok(sound) => sound,
        Err(error) => {
            tracing::warn!(%error, path, "could not load feedback sound; using default");
            bundled.clone()
        }
    }
}

#[cfg(target_os = "macos")]
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn set_volume(volume: f32) {
    VOLUME.store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
}

fn volume() -> f32 {
    f32::from_bits(VOLUME.load(Ordering::Relaxed))
}

fn sounds_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed) && volume() > 0.0
}

// Only the playback worker owns a device. Turning sounds off drops it within
// one observation interval, and opening/retrying never blocks capture or UI.
struct FeedbackOutput<S> {
    sink: Option<S>,
    retry_at: Instant,
}

impl<S> FeedbackOutput<S> {
    fn new() -> Self {
        Self {
            sink: None,
            retry_at: Instant::now(),
        }
    }

    fn update(
        &mut self,
        enabled: bool,
        now: Instant,
        open: impl FnOnce() -> Result<S>,
    ) -> Result<()> {
        if !enabled {
            self.sink = None;
            self.retry_at = now;
        } else if self.sink.is_none() && now >= self.retry_at {
            self.retry_at = now + Duration::from_secs(2);
            self.sink = Some(open()?);
        }
        Ok(())
    }
}

/// Maps the admission wait outcome to an error. A timeout does not stop the
/// loader thread: the audio stack may simply be cold, and the loader keeps
/// running so tones recover once the default output finally opens.
fn admission_error(outcome: Result<Result<()>, mpsc::RecvTimeoutError>) -> Result<()> {
    match outcome {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(eyre!("timed out preloading feedback audio")),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(eyre!("feedback audio loader exited before initializing"))
        }
    }
}

fn publish_player(
    player: &OnceLock<SyncSender<Tone>>,
    sender: SyncSender<Tone>,
    ready: SyncSender<Result<()>>,
    outcome: Result<()>,
) {
    let _ = player.set(sender);
    let _ = ready.send(outcome);
}

pub fn preload() -> Result<()> {
    if DICTATION_PLAYER.get().is_some() {
        return Ok(());
    }
    if LOADER_STARTED.swap(true, Ordering::AcqRel) {
        // A loader is already running from an earlier admission; it registers
        // the player itself, so report the same slow-audio outcome.
        return Err(eyre!("feedback audio is still initializing"));
    }
    let (sender, receiver) = mpsc::sync_channel(8);
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let result = (|| -> Result<_> {
            let start = decode(include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/resources/audio/startRecording.mp3"
            )))?;
            let stop = decode(include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/resources/audio/stopRecording.mp3"
            )))?;
            let cancel = decode(include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/resources/audio/cancel.mp3"
            )))?;
            Ok((start, stop, cancel))
        })();
        let Ok((start, stop, cancel)) = result else {
            let _ = ready_sender.send(result.map(|_| ()));
            LOADER_STARTED.store(false, Ordering::Release);
            return;
        };
        let mut output = FeedbackOutput::new();
        let mut generation = SOUND_GENERATION.load(Ordering::Acquire);
        let mut selection = sound_selection();
        let mut start_sound = resolve_sound(&selection.start, &start);
        let mut stop_sound = resolve_sound(&selection.stop, &stop);
        let open =
            || DeviceSinkBuilder::open_default_sink().wrap_err("could not open the audio output");
        let initial = output.update(sounds_enabled(), Instant::now(), open);
        // Publish even after an admission timeout or output-open failure. The
        // same worker keeps observing preferences and retrying the device.
        publish_player(&DICTATION_PLAYER, sender, ready_sender, initial);
        loop {
            let tone = match receiver.recv_timeout(Duration::from_millis(250)) {
                Ok(tone) => Some(tone),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if let Err(error) = output.update(sounds_enabled(), Instant::now(), open) {
                tracing::warn!(%error, "recording audio output unavailable; retrying");
            }
            // Opening may have been slow; recheck the latest preference before
            // retaining the output or playing any queued sound.
            if !sounds_enabled() {
                let _ = output.update(false, Instant::now(), open);
                continue;
            }
            let current_generation = SOUND_GENERATION.load(Ordering::Acquire);
            if current_generation != generation {
                generation = current_generation;
                selection = sound_selection();
                start_sound = resolve_sound(&selection.start, &start);
                stop_sound = resolve_sound(&selection.stop, &stop);
            }
            let (Some(tone), Some(sink)) = (tone, output.sink.as_ref()) else {
                continue;
            };
            let sound = match tone {
                Tone::DictationStart => &start_sound,
                Tone::DictationStop => &stop_sound,
                Tone::Cancel => &cancel,
                #[cfg(target_os = "macos")]
                Tone::Error => continue,
            };
            sink.mixer().add(sound.clone().amplify(volume()));
        }
    });
    let outcome = ready_receiver.recv_timeout(Duration::from_secs(2));
    admission_error(outcome)?;
    Ok(())
}

pub fn play(tone: Tone) {
    if !sounds_enabled() {
        return;
    }
    match tone {
        Tone::DictationStart | Tone::DictationStop | Tone::Cancel => {
            enqueue(DICTATION_PLAYER.get(), tone);
        }
        #[cfg(target_os = "macos")]
        Tone::Error => play_system_sound(tone),
    }
}

fn enqueue(player: Option<&SyncSender<Tone>>, tone: Tone) {
    if let Some(player) = player {
        let _ = player.try_send(tone);
    }
}

#[cfg(target_os = "macos")]
fn play_system_sound(tone: Tone) {
    let sound = match tone {
        Tone::Error => "Basso",
        Tone::DictationStart | Tone::DictationStop | Tone::Cancel => return,
    };
    let volume = volume().to_string();
    let child = Command::new("/usr/bin/afplay")
        .args([
            "-v",
            &volume,
            &format!("/System/Library/Sounds/{sound}.aiff"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = child {
        thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

fn decode(bytes: &'static [u8]) -> Result<SamplesBuffer> {
    decode_bytes(bytes.to_vec())
}

fn decode_bytes(bytes: Vec<u8>) -> Result<SamplesBuffer> {
    let decoder = Decoder::new(Cursor::new(bytes)).wrap_err("could not decode feedback audio")?;
    let channels = decoder.channels();
    let sample_rate = decoder.sample_rate();
    Ok(SamplesBuffer::new(
        channels,
        sample_rate,
        decoder.collect::<Vec<_>>(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_sounds_decode_as_aiff() {
        for name in SYSTEM_SOUNDS {
            let bytes = std::fs::read(format!("/System/Library/Sounds/{name}.aiff")).unwrap();
            let sound = decode_bytes(bytes).unwrap();
            assert!(sound.total_duration().unwrap() > Duration::ZERO, "{name}");
        }
    }

    #[test]
    fn timed_out_admission_still_publishes_a_usable_player() {
        let player = OnceLock::new();
        let (sender, receiver) = mpsc::sync_channel(8);
        let (ready, waiting) = mpsc::sync_channel(1);
        assert!(admission_error(waiting.recv_timeout(Duration::ZERO)).is_err());
        drop(waiting);
        publish_player(&player, sender, ready, Ok(()));
        enqueue(player.get(), Tone::DictationStart);
        assert_eq!(receiver.try_recv().unwrap(), Tone::DictationStart);
    }

    #[test]
    fn sound_off_releases_output_and_reenable_reopens_it() {
        use std::cell::Cell;
        struct Sink<'a>(&'a Cell<usize>);
        impl Drop for Sink<'_> {
            fn drop(&mut self) {
                self.0.set(self.0.get() - 1);
            }
        }
        let live = Cell::new(0);
        let opens = Cell::new(0);
        let open = || {
            live.set(live.get() + 1);
            opens.set(opens.get() + 1);
            Ok(Sink(&live))
        };
        let mut output = FeedbackOutput::new();
        let now = Instant::now();
        output.update(false, now, open).unwrap();
        assert_eq!(opens.get(), 0);
        output.update(true, now, open).unwrap();
        output.update(true, now, open).unwrap();
        assert_eq!(opens.get(), 1);
        assert_eq!(live.get(), 1);
        output.update(false, now, open).unwrap();
        assert_eq!(live.get(), 0);
        output.update(true, now, open).unwrap();
        assert_eq!(opens.get(), 2);
        drop(output);
        assert_eq!(live.get(), 0);
    }

    #[test]
    fn failed_output_initialization_retries_with_a_bounded_backoff() {
        let mut output = FeedbackOutput::new();
        let now = Instant::now();
        assert!(
            output
                .update(true, now, || Err(eyre!("device unavailable")))
                .is_err()
        );
        output
            .update(true, now + Duration::from_secs(1), || -> Result<()> {
                panic!("must not spin on device failure")
            })
            .unwrap();
        assert!(output.sink.is_none());
        output
            .update(true, now + Duration::from_secs(2), || Ok(()))
            .unwrap();
        assert!(output.sink.is_some());
    }

    #[test]
    fn bundled_recording_sounds_decode_without_an_audio_device() {
        for bytes in [
            include_bytes!("../resources/audio/startRecording.mp3").as_slice(),
            include_bytes!("../resources/audio/stopRecording.mp3").as_slice(),
            include_bytes!("../resources/audio/cancel.mp3").as_slice(),
        ] {
            let mut sound = decode(bytes).unwrap();
            assert!(sound.total_duration().unwrap() > Duration::ZERO);
            assert!(sound.any(|sample| sample.abs() > 0.0));
        }
    }

    #[test]
    fn feedback_admission_never_waits_for_playback() {
        let (sender, receiver) = mpsc::sync_channel(2);
        enqueue(Some(&sender), Tone::DictationStart);
        enqueue(Some(&sender), Tone::DictationStop);
        enqueue(Some(&sender), Tone::Cancel);
        assert_eq!(receiver.try_recv().unwrap(), Tone::DictationStart);
        assert_eq!(receiver.try_recv().unwrap(), Tone::DictationStop);
        assert!(receiver.try_recv().is_err());
        drop(receiver);
        enqueue(Some(&sender), Tone::Cancel);
        enqueue(None, Tone::DictationStart);
    }

    #[test]
    fn timeout_and_loader_exit_report_distinct_errors() {
        // A silent channel models a loader still warming the audio stack; a
        // dropped one models a loader that exited before loading.
        assert_eq!(
            admission_error(Err(mpsc::RecvTimeoutError::Timeout))
                .unwrap_err()
                .to_string(),
            "timed out preloading feedback audio"
        );
        assert_eq!(
            admission_error(Err(mpsc::RecvTimeoutError::Disconnected))
                .unwrap_err()
                .to_string(),
            "feedback audio loader exited before initializing"
        );

        // Real channels exercise the same admission wait the loader drives.
        let (slow_sender, slow_receiver) = mpsc::sync_channel::<Result<()>>(1);
        assert!(admission_error(slow_receiver.recv_timeout(Duration::ZERO)).is_err());
        drop(slow_sender);

        let (done_sender, done_receiver) = mpsc::sync_channel::<Result<()>>(1);
        done_sender.send(Ok(())).unwrap();
        assert!(admission_error(done_receiver.recv_timeout(Duration::ZERO)).is_ok());

        let (failed_sender, failed_receiver) = mpsc::sync_channel::<Result<()>>(1);
        failed_sender
            .send(Err(eyre!("could not open the audio output")))
            .unwrap();
        assert_eq!(
            admission_error(failed_receiver.recv_timeout(Duration::ZERO))
                .unwrap_err()
                .to_string(),
            "could not open the audio output"
        );
    }
}
