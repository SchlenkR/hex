use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use color_eyre::Result;

use crate::audio::CaptureInstant;
use crate::context::{ContextMonitor, ContextSnapshot};
use crate::dictation::Finish;
use crate::dictation_audio::{DictationAudio, DictationAudioEvent};
use crate::dictation_indicator::{DictationIndicatorEvent, DictationIndicatorSender};
use crate::events::{DictationPhase, EventLog, VoiceEvent, VoiceState, now_ms};
use crate::feedback::{self, Tone};
use crate::parakeet::{
    DictationJobStage, DictationWorker, PasteKind, TranscriptionTarget, WorkerEvent,
};
use crate::recording_environment::RecordingEnvironmentController;
use crate::suppression::{DictationHotkey, HotkeyAction, InputMonitor};

const PROGRAMMATIC_LEASE: Duration = Duration::from_secs(10);
const PROGRAMMATIC_COMPLETION_LIMIT: usize = 16;

#[derive(Debug)]
pub enum RecognitionControl {
    PasteLast,
    StartDictation {
        source: String,
        owner_token: String,
        levels: SyncSender<DictationLevel>,
        reply: SyncSender<Result<ProgrammaticDictationStart, String>>,
    },
    AttachDictationAudio {
        id: u64,
        owner_token: String,
        audio: SyncSender<DictationAudioChunk>,
        reply: SyncSender<Result<(), String>>,
    },
    FinishDictation {
        id: u64,
        owner_token: String,
        reply: SyncSender<Result<ProgrammaticDictationResult, String>>,
    },
    CancelDictation {
        id: u64,
        owner_token: String,
        reply: SyncSender<Result<(), String>>,
    },
    HeartbeatDictation {
        id: u64,
        owner_token: String,
        reply: SyncSender<Result<(), String>>,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct DictationLevel {
    pub rms_db: f32,
    pub peak_db: f32,
}

#[derive(Clone, Debug)]
pub struct ProgrammaticDictationResult {
    pub transcript: String,
    pub duration_ms: u64,
}

#[derive(Debug)]
pub struct ProgrammaticDictationStart {
    pub id: u64,
    pub sample_rate: u32,
}

#[derive(Debug)]
pub struct DictationAudioChunk {
    pub samples: Vec<f32>,
}

struct ProgrammaticDictation {
    id: u64,
    owner_token: String,
    source: String,
    levels: SyncSender<DictationLevel>,
    audio: Option<SyncSender<DictationAudioChunk>>,
    last_level: Instant,
    lease_deadline: Instant,
}

struct FinishingProgrammaticDictation {
    id: u64,
    owner_token: String,
    duration_ms: u64,
    replies: Vec<SyncSender<Result<ProgrammaticDictationResult, String>>>,
}

#[derive(Clone)]
enum ProgrammaticTerminalOutcome {
    Finished(Result<ProgrammaticDictationResult, String>),
    Cancelled,
}

#[derive(Clone)]
struct ProgrammaticCompletion {
    id: u64,
    owner_token: String,
    outcome: ProgrammaticTerminalOutcome,
}

pub fn listen(
    events: EventLog,
    device_override: Option<&str>,
    shutdown: &AtomicBool,
    indicator: Option<DictationIndicatorSender>,
    history: Option<crate::history::History>,
    controls: Option<Receiver<RecognitionControl>>,
) -> Result<()> {
    shutdown.store(false, Ordering::Relaxed);
    // Feedback admission is advisory: a cold audio stack can exceed the
    // preload timeout, and missing tones must not stop desktop recognition.
    if let Err(error) = feedback::preload() {
        tracing::warn!(%error, "recording sounds are unavailable; continuing without feedback");
    }
    let mut release_while_idle = crate::app_settings::release_microphone_while_idle();
    let mut events = events;
    let input_monitor = InputMonitor::start()?;
    let context_monitor = ContextMonitor::start();
    let mut context = ContextSnapshot::default();
    let mut hotkey = DictationHotkey::new_with_bindings(
        CaptureInstant::now(),
        crate::app_settings::double_tap_lock(),
        crate::app_settings::dictation_hotkeys(),
    );
    hotkey.set_double_tap_only(crate::app_settings::double_tap_only());
    let recording_environment = RecordingEnvironmentController::start();
    let (mut microphone_revision, microphone) = crate::app_settings::microphone_selection();
    let input = DictationAudio::open(
        device_override,
        microphone_revision,
        microphone.as_deref(),
        recording_environment,
        input_monitor.pending_events(),
        release_while_idle,
    )?;
    let dictation_worker = DictationWorker::start(input_monitor.activity.clone(), history);
    let (mut transcription_revision, _) = crate::app_settings::transcription_selection();
    let mut next_programmatic_id = 1_u64;
    let mut programmatic = None::<ProgrammaticDictation>;
    let mut programmatic_results =
        BTreeMap::<crate::parakeet::DictationJobId, FinishingProgrammaticDictation>::new();
    let mut programmatic_completions = VecDeque::<ProgrammaticCompletion>::new();

    events.emit(&VoiceEvent::SessionStarted {
        timestamp_ms: now_ms(),
    })?;
    if input.is_recovering() {
        hotkey.suspend();
    }
    if hotkey.is_recording() {
        let accepted = input.start(if release_while_idle {
            CaptureInstant::now()
        } else {
            input.captured_through()
        })?;
        if accepted {
            events.dictation(DictationPhase::Started, "")?;
            if let Some(indicator) = &indicator {
                indicator.send(DictationIndicatorEvent::Started);
            }
        } else {
            hotkey.suspend();
        }
    }
    emit_state(&mut events, hotkey.is_recording(), &input.device_name())?;
    tracing::info!(device = %input.device_name(), "dictation listener started");

    while !shutdown.load(Ordering::Relaxed) {
        if let Some(controls) = &controls {
            while let Ok(control) = controls.try_recv() {
                match control {
                    RecognitionControl::PasteLast if programmatic.is_none() => {
                        handle_hotkey_action(
                            HotkeyAction::PasteLast,
                            CaptureInstant::now(),
                            &input,
                            &dictation_worker,
                            &context,
                            &input.device_name(),
                            &mut events,
                            indicator.as_ref(),
                        )?;
                    }
                    RecognitionControl::PasteLast => {}
                    RecognitionControl::StartDictation {
                        source,
                        owner_token,
                        levels,
                        reply,
                    } => {
                        if programmatic.is_some() || input.is_recording() || hotkey.is_recording() {
                            let _ = reply.send(Err("dictation-busy".into()));
                            continue;
                        }
                        let id = next_programmatic_id;
                        next_programmatic_id = next_programmatic_id.wrapping_add(1).max(1);
                        let boundary = if release_while_idle {
                            CaptureInstant::now()
                        } else {
                            input.captured_through()
                        };
                        match input.start_programmatic(boundary) {
                            Ok(()) => {
                                hotkey.disarm_pending_gesture();
                                tracing::info!(
                                    dictation_id = id,
                                    source,
                                    "programmatic dictation started"
                                );
                                programmatic = Some(ProgrammaticDictation {
                                    id,
                                    owner_token,
                                    source,
                                    levels,
                                    audio: None,
                                    last_level: Instant::now() - Duration::from_millis(30),
                                    lease_deadline: Instant::now() + PROGRAMMATIC_LEASE,
                                });
                                input_monitor.set_escape_cancels(true);
                                let _ = reply.send(Ok(ProgrammaticDictationStart {
                                    id,
                                    sample_rate: input.sample_rate(),
                                }));
                            }
                            Err(error) => {
                                let _ = reply.send(Err(error.to_string()));
                            }
                        }
                    }
                    RecognitionControl::AttachDictationAudio {
                        id,
                        owner_token,
                        audio,
                        reply,
                    } => {
                        let Some(active) = programmatic
                            .as_mut()
                            .filter(|active| active.id == id && active.owner_token == owner_token)
                        else {
                            let _ = reply.send(Err("dictation-not-active".into()));
                            continue;
                        };
                        if active.audio.is_some() {
                            let _ = reply.send(Err("audio-already-subscribed".into()));
                            continue;
                        }
                        active.audio = Some(audio);
                        let _ = reply.send(Ok(()));
                    }
                    RecognitionControl::FinishDictation {
                        id,
                        owner_token,
                        reply,
                    } => {
                        if let Some(finishing) =
                            programmatic_results.values_mut().find(|finishing| {
                                finishing.id == id && finishing.owner_token == owner_token
                            })
                        {
                            finishing.replies.push(reply);
                            continue;
                        }
                        if let Some(completion) =
                            programmatic_completions.iter().rev().find(|completion| {
                                completion.id == id && completion.owner_token == owner_token
                            })
                        {
                            let result = match &completion.outcome {
                                ProgrammaticTerminalOutcome::Finished(result) => result.clone(),
                                ProgrammaticTerminalOutcome::Cancelled => Err("cancelled".into()),
                            };
                            let _ = reply.send(result);
                            continue;
                        }
                        if programmatic.as_ref().is_none_or(|active| {
                            active.id != id || active.owner_token != owner_token
                        }) {
                            let _ = reply.send(Err("dictation-not-active".into()));
                            continue;
                        }
                        let active = programmatic.take().expect("active dictation was checked");
                        let boundary = if release_while_idle {
                            CaptureInstant::now()
                        } else {
                            input.captured_through()
                        };
                        match input.finish(boundary)? {
                            Finish::Transcribe(clip) => {
                                let duration_ms = clip.duration_ms();
                                match dictation_worker.transcribe(
                                    clip,
                                    TranscriptionTarget::Service,
                                    ContextSnapshot::default(),
                                ) {
                                    Ok(job_id) => {
                                        programmatic_results.insert(
                                            job_id,
                                            FinishingProgrammaticDictation {
                                                id,
                                                owner_token,
                                                duration_ms,
                                                replies: vec![reply],
                                            },
                                        );
                                        tracing::info!(
                                            dictation_id = id,
                                            source = active.source,
                                            "programmatic dictation submitted"
                                        );
                                    }
                                    Err(error) => {
                                        let result = Err(error.into());
                                        let _ = reply.send(result.clone());
                                        remember_programmatic_completion(
                                            &mut programmatic_completions,
                                            ProgrammaticCompletion {
                                                id,
                                                owner_token,
                                                outcome: ProgrammaticTerminalOutcome::Finished(
                                                    result,
                                                ),
                                            },
                                        );
                                    }
                                }
                            }
                            Finish::Discard => {
                                let result = Err("capture-discarded".into());
                                let _ = reply.send(result.clone());
                                remember_programmatic_completion(
                                    &mut programmatic_completions,
                                    ProgrammaticCompletion {
                                        id,
                                        owner_token,
                                        outcome: ProgrammaticTerminalOutcome::Finished(result),
                                    },
                                );
                            }
                        }
                    }
                    RecognitionControl::CancelDictation {
                        id,
                        owner_token,
                        reply,
                    } => {
                        if let Some(completion) =
                            programmatic_completions.iter().rev().find(|completion| {
                                completion.id == id && completion.owner_token == owner_token
                            })
                            && matches!(completion.outcome, ProgrammaticTerminalOutcome::Cancelled)
                        {
                            let _ = reply.send(Ok(()));
                            continue;
                        }
                        if programmatic_results.values().any(|finishing| {
                            finishing.id == id && finishing.owner_token == owner_token
                        }) {
                            let _ = reply.send(Err("dictation-finishing".into()));
                            continue;
                        }
                        if programmatic.as_ref().is_none_or(|active| {
                            active.id != id || active.owner_token != owner_token
                        }) {
                            let _ = reply.send(Err("dictation-not-active".into()));
                            continue;
                        }
                        let active = programmatic.take().expect("active dictation was checked");
                        input.cancel()?;
                        tracing::info!(
                            dictation_id = id,
                            source = active.source,
                            "programmatic dictation cancelled"
                        );
                        remember_programmatic_completion(
                            &mut programmatic_completions,
                            ProgrammaticCompletion {
                                id,
                                owner_token,
                                outcome: ProgrammaticTerminalOutcome::Cancelled,
                            },
                        );
                        let _ = reply.send(Ok(()));
                    }
                    RecognitionControl::HeartbeatDictation {
                        id,
                        owner_token,
                        reply,
                    } => {
                        let Some(active) = programmatic
                            .as_mut()
                            .filter(|active| active.id == id && active.owner_token == owner_token)
                        else {
                            let _ = reply.send(Err("dictation-not-active".into()));
                            continue;
                        };
                        active.lease_deadline = Instant::now() + PROGRAMMATIC_LEASE;
                        let _ = reply.send(Ok(()));
                    }
                }
            }
        }
        if programmatic
            .as_ref()
            .is_some_and(|active| programmatic_lease_expired(active.lease_deadline, Instant::now()))
        {
            let expired = programmatic.take().expect("expired dictation was checked");
            input.cancel()?;
            tracing::warn!(
                dictation_id = expired.id,
                source = expired.source,
                "programmatic dictation lease expired"
            );
            remember_programmatic_completion(
                &mut programmatic_completions,
                ProgrammaticCompletion {
                    id: expired.id,
                    owner_token: expired.owner_token,
                    outcome: ProgrammaticTerminalOutcome::Cancelled,
                },
            );
        }
        while let Ok(next_context) = context_monitor.updates.try_recv() {
            if context != next_context {
                input_monitor.activity.invalidate();
            }
            context = next_context;
            events.emit(&VoiceEvent::Context {
                timestamp_ms: now_ms(),
                application: context.application.clone(),
                browser_url: context.browser_url.as_ref().map(ToString::to_string),
            })?;
        }
        let next_release_while_idle = crate::app_settings::release_microphone_while_idle();
        if next_release_while_idle != release_while_idle {
            release_while_idle = next_release_while_idle;
            input.set_release_while_idle(release_while_idle);
        }
        hotkey.set_double_tap_enabled(crate::app_settings::double_tap_lock());
        hotkey.set_double_tap_only(crate::app_settings::double_tap_only());
        hotkey.set_bindings(crate::app_settings::dictation_hotkeys());
        let (next_microphone_revision, microphone) = crate::app_settings::microphone_selection();
        if next_microphone_revision != microphone_revision {
            input.request_selection(next_microphone_revision, microphone.as_deref());
            microphone_revision = next_microphone_revision;
        }
        let (next_revision, selection) = crate::app_settings::transcription_selection();
        if next_revision != transcription_revision && dictation_worker.reload(selection).is_ok() {
            transcription_revision = next_revision;
        }
        let hotkey_capture_active = crate::app_settings::hotkey_capture_active();
        let hotkey_capture_suspended = hotkey_capture_active || input.is_recovering();
        if hotkey_capture_suspended && hotkey.suspend().is_some() {
            handle_hotkey_action(
                HotkeyAction::Cancel,
                input.captured_through(),
                &input,
                &dictation_worker,
                &context,
                &input.device_name(),
                &mut events,
                indicator.as_ref(),
            )?;
        }
        while let Ok(observed) = input_monitor.events.try_recv() {
            let _acknowledge = input_monitor.acknowledge_after(observed);
            let input_event = observed.event;
            let capture_at = observed.capture_at;
            if programmatic.is_some() {
                hotkey.track_key_state(input_event, capture_at);
            }
            if programmatic.is_some() && input_event.is_escape_down() {
                let cancelled = programmatic
                    .take()
                    .expect("programmatic dictation is active");
                input.cancel()?;
                feedback::play(Tone::Cancel);
                remember_programmatic_completion(
                    &mut programmatic_completions,
                    ProgrammaticCompletion {
                        id: cancelled.id,
                        owner_token: cancelled.owner_token,
                        outcome: ProgrammaticTerminalOutcome::Cancelled,
                    },
                );
                continue;
            } else if programmatic.is_some() {
                continue;
            }
            if hotkey_capture_suspended {
                continue;
            }
            if !hotkey.is_recording()
                && input_event.is_escape_down()
                && let Some(job_id) = dictation_worker.cancel_latest()
            {
                feedback::play(Tone::Cancel);
                events.dictation(DictationPhase::Cancelled, "")?;
                if let Some(indicator) = &indicator {
                    indicator.send(DictationIndicatorEvent::JobCancelled {
                        job_id: job_id.value(),
                    });
                }
                continue;
            }
            if let Some(action) = hotkey.process(input_event, capture_at) {
                let accepted = handle_hotkey_action(
                    action,
                    capture_at,
                    &input,
                    &dictation_worker,
                    &context,
                    &input.device_name(),
                    &mut events,
                    indicator.as_ref(),
                )?;
                if !accepted {
                    hotkey.suspend();
                }
            }
        }
        if programmatic.is_none()
            && !hotkey_capture_suspended
            && input_monitor.pending_events().oldest().is_none()
        {
            hotkey.recover_stale_keys();
        }
        if hotkey.is_recording() {
            input.become_intentional(if release_while_idle {
                CaptureInstant::now()
            } else {
                input.captured_through()
            })?;
        }
        let mut received_worker_event = false;
        while let Some(event) = dictation_worker.try_recv() {
            received_worker_event = true;
            match event {
                WorkerEvent::Completed {
                    job_id,
                    target: TranscriptionTarget::Service,
                    result,
                    ..
                } => {
                    if let Some(finishing) = programmatic_results.remove(&job_id) {
                        let result = result.map(|transcript| ProgrammaticDictationResult {
                            transcript,
                            duration_ms: finishing.duration_ms,
                        });
                        for reply in finishing.replies {
                            let _ = reply.send(result.clone());
                        }
                        remember_programmatic_completion(
                            &mut programmatic_completions,
                            ProgrammaticCompletion {
                                id: finishing.id,
                                owner_token: finishing.owner_token,
                                outcome: ProgrammaticTerminalOutcome::Finished(result),
                            },
                        );
                    }
                    continue;
                }
                WorkerEvent::Cancelled { job_id } => {
                    if let Some(finishing) = programmatic_results.remove(&job_id) {
                        let result = Err("cancelled".into());
                        for reply in finishing.replies {
                            let _ = reply.send(result.clone());
                        }
                        remember_programmatic_completion(
                            &mut programmatic_completions,
                            ProgrammaticCompletion {
                                id: finishing.id,
                                owner_token: finishing.owner_token,
                                outcome: ProgrammaticTerminalOutcome::Finished(result),
                            },
                        );
                        continue;
                    }
                    handle_dictation_event(
                        WorkerEvent::Cancelled { job_id },
                        &mut events,
                        indicator.as_ref(),
                    )?;
                    continue;
                }
                event => handle_dictation_event(event, &mut events, indicator.as_ref())?,
            }
        }
        input_monitor.set_escape_cancels(
            hotkey.is_recording() || programmatic.is_some() || dictation_worker.pending_count() > 0,
        );
        if received_worker_event {
            emit_engine_state(
                &mut events,
                hotkey.is_recording(),
                &dictation_worker,
                &input.device_name(),
            )?;
        }

        while let Some(audio_event) = input.try_recv_event() {
            match audio_event {
                DictationAudioEvent::ReadyIntentional { .. } => {}
                DictationAudioEvent::OpenFailed { error, .. } => {
                    programmatic = None;
                    hotkey.suspend();
                    feedback::play(Tone::Error);
                    events.dictation(
                        DictationPhase::Failed(format!("Could not open microphone: {error}")),
                        "",
                    )?;
                    if let Some(indicator) = &indicator {
                        indicator.send(DictationIndicatorEvent::Failed);
                    }
                    emit_engine_state(&mut events, false, &dictation_worker, &input.device_name())?;
                }
                DictationAudioEvent::RecognitionDiscontinuity { dropped_frames } => {
                    tracing::warn!(
                        dropped_recognition_frames = dropped_frames,
                        dropped_recognition_ms =
                            dropped_frames * 1_000 / u64::from(input.sample_rate()),
                        "audio monitoring fell behind; dictation capture remained continuous"
                    );
                    input.discard_recognition_backlog();
                }
                DictationAudioEvent::CaptureDiscontinuity {
                    was_recording,
                    gap_ms,
                    ..
                } => {
                    input.discard_recognition_backlog();
                    if was_recording {
                        programmatic = None;
                        hotkey.suspend();
                        feedback::play(Tone::Error);
                        events.dictation(
                            DictationPhase::Failed(format!(
                                "Microphone audio was interrupted for {gap_ms} ms."
                            )),
                            "",
                        )?;
                        if let Some(indicator) = &indicator {
                            indicator.send(DictationIndicatorEvent::Failed);
                        }
                        emit_engine_state(
                            &mut events,
                            false,
                            &dictation_worker,
                            &input.device_name(),
                        )?;
                    }
                }
                DictationAudioEvent::Reopened => {
                    input.discard_recognition_backlog();
                    if !input.is_recording() {
                        emit_state(&mut events, false, &input.device_name())?;
                    }
                }
                DictationAudioEvent::Interrupted { was_recording, .. } => {
                    if was_recording {
                        programmatic = None;
                    }
                    hotkey.suspend();
                    input.discard_recognition_backlog();
                    if was_recording {
                        feedback::play(Tone::Error);
                        events.dictation(
                            DictationPhase::Failed(
                                "Microphone capture was interrupted; reconnecting.".into(),
                            ),
                            "",
                        )?;
                        if let Some(indicator) = &indicator {
                            indicator.send(DictationIndicatorEvent::Failed);
                        }
                        emit_engine_state(
                            &mut events,
                            false,
                            &dictation_worker,
                            &input.device_name(),
                        )?;
                    }
                }
            }
        }

        let audio = input.recv_timeout(Duration::from_millis(20))?;
        if let (Some(active), Some(audio)) = (&mut programmatic, &audio) {
            if active.last_level.elapsed() >= Duration::from_millis(30) {
                let _ = active.levels.try_send(level_for(&audio.samples));
                active.last_level = Instant::now();
            }
            if let Some(sender) = &active.audio {
                match sender.try_send(DictationAudioChunk {
                    samples: audio.samples.clone(),
                }) {
                    Ok(()) | Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => active.audio = None,
                }
            }
        }
        if let Some(audio) = &audio
            && audio.is_current(input.recognition_generation())
            && (programmatic.is_some() || hotkey.suppresses_recognition())
            && let Some(indicator) = &indicator
        {
            indicator.meter(&audio.samples);
        }
    }

    events.emit(&VoiceEvent::State {
        timestamp_ms: now_ms(),
        state: VoiceState::Stopping,
        device: input.device_name(),
    })?;
    events.flush()?;
    if let Some(indicator) = &indicator {
        indicator.send(DictationIndicatorEvent::Discarded);
    }
    Ok(())
}

fn level_for(samples: &[f32]) -> DictationLevel {
    let (sum_squares, peak) = samples
        .iter()
        .fold((0.0_f64, 0.0_f32), |(sum, peak), sample| {
            let magnitude = sample.abs();
            (
                sum + f64::from(*sample) * f64::from(*sample),
                peak.max(magnitude),
            )
        });
    let rms = if samples.is_empty() {
        0.0
    } else {
        (sum_squares / samples.len() as f64).sqrt() as f32
    };
    DictationLevel {
        rms_db: amplitude_db(rms),
        peak_db: amplitude_db(peak),
    }
}

fn remember_programmatic_completion(
    completions: &mut VecDeque<ProgrammaticCompletion>,
    completion: ProgrammaticCompletion,
) {
    completions.push_back(completion);
    while completions.len() > PROGRAMMATIC_COMPLETION_LIMIT {
        completions.pop_front();
    }
}

fn programmatic_lease_expired(deadline: Instant, now: Instant) -> bool {
    now >= deadline
}

fn amplitude_db(amplitude: f32) -> f32 {
    (20.0 * amplitude.max(0.000_001).log10()).max(-120.0)
}

#[allow(clippy::too_many_arguments)]
fn handle_hotkey_action(
    action: HotkeyAction,
    action_at: CaptureInstant,
    dictation: &DictationAudio,
    worker: &DictationWorker,
    context: &ContextSnapshot,
    device: &str,
    events: &mut EventLog,
    indicator: Option<&DictationIndicatorSender>,
) -> Result<bool> {
    match action {
        HotkeyAction::Start => {
            if !dictation.start(action_at)? {
                return Ok(false);
            }
            feedback::play(Tone::DictationStart);
            worker.prepare_paste();
            events.dictation(DictationPhase::Started, "")?;
            if let Some(indicator) = indicator {
                indicator.send(DictationIndicatorEvent::Started);
            }
            emit_state(events, true, device)
        }
        HotkeyAction::Finish => {
            dictation.become_intentional(action_at)?;
            finish_dictation(
                dictation,
                action_at,
                worker,
                TranscriptionTarget::Paste,
                context,
                device,
                events,
                indicator,
            )?;
            emit_engine_state(events, false, worker, device)
        }
        HotkeyAction::Discard => {
            dictation.cancel()?;
            events.dictation(DictationPhase::Discarded, "")?;
            if let Some(indicator) = indicator {
                indicator.send(DictationIndicatorEvent::Discarded);
            }
            emit_state(events, false, device)
        }
        HotkeyAction::Cancel => {
            dictation.cancel()?;
            feedback::play(Tone::Cancel);
            events.dictation(DictationPhase::Cancelled, "")?;
            if let Some(indicator) = indicator {
                indicator.send(DictationIndicatorEvent::Cancelled);
            }
            emit_state(events, false, device)
        }
        HotkeyAction::PasteLast => {
            dictation.cancel()?;
            if let Err(error) = worker.paste_last() {
                feedback::play(Tone::Error);
                events.dictation(DictationPhase::Failed(error.into()), "")?;
            }
            emit_engine_state(events, false, worker, device)
        }
    }?;
    Ok(true)
}

fn emit_state(events: &mut EventLog, suppressed: bool, device: &str) -> Result<()> {
    events.emit(&VoiceEvent::State {
        timestamp_ms: now_ms(),
        state: if suppressed {
            VoiceState::Dictating
        } else {
            VoiceState::Listening
        },
        device: device.into(),
    })?;
    Ok(())
}

fn emit_engine_state(
    events: &mut EventLog,
    capturing: bool,
    worker: &DictationWorker,
    device: &str,
) -> Result<()> {
    events.emit(&VoiceEvent::State {
        timestamp_ms: now_ms(),
        state: if capturing {
            VoiceState::Dictating
        } else if worker.is_busy() {
            VoiceState::Transcribing
        } else {
            VoiceState::Listening
        },
        device: device.into(),
    })?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn finish_dictation(
    capture: &DictationAudio,
    ended_at: CaptureInstant,
    worker: &DictationWorker,
    target: TranscriptionTarget,
    context: &ContextSnapshot,
    device: &str,
    events: &mut EventLog,
    indicator: Option<&DictationIndicatorSender>,
) -> Result<()> {
    let Finish::Transcribe(clip) = capture.finish(ended_at)? else {
        events.dictation(DictationPhase::Discarded, "")?;
        if let Some(indicator) = indicator {
            indicator.send(DictationIndicatorEvent::Discarded);
        }
        return Ok(());
    };
    feedback::play(Tone::DictationStop);
    events.emit(&VoiceEvent::State {
        timestamp_ms: now_ms(),
        state: VoiceState::Transcribing,
        device: device.into(),
    })?;
    events.dictation(DictationPhase::Transcribing, "")?;
    match worker.transcribe(clip, target, context.clone()) {
        Ok(job_id) => {
            if let Some(indicator) = indicator {
                indicator.send(DictationIndicatorEvent::Submitted {
                    job_id: job_id.value(),
                });
            }
        }
        Err(error) => {
            feedback::play(Tone::Error);
            events.dictation(DictationPhase::Failed(error.into()), "")?;
            if let Some(indicator) = indicator {
                indicator.send(DictationIndicatorEvent::Failed);
            }
        }
    }
    Ok(())
}

fn handle_dictation_event(
    event: WorkerEvent,
    events: &mut EventLog,
    indicator: Option<&DictationIndicatorSender>,
) -> Result<()> {
    match event {
        WorkerEvent::Completed {
            job_id,
            result: Ok(text),
            ..
        } if text.trim().is_empty() => {
            events.dictation(DictationPhase::Discarded, "")?;
            if let Some(indicator) = indicator {
                indicator.send(DictationIndicatorEvent::JobCompleted {
                    job_id: job_id.value(),
                });
            }
        }
        WorkerEvent::Completed {
            job_id,
            target,
            result: Ok(text),
        } => {
            let phase = match target {
                TranscriptionTarget::Paste => DictationPhase::Pasted,
                TranscriptionTarget::Service => return Ok(()),
            };
            events.dictation(phase, text)?;
            if let Some(indicator) = indicator {
                indicator.send(DictationIndicatorEvent::JobCompleted {
                    job_id: job_id.value(),
                });
            }
        }
        WorkerEvent::Completed {
            job_id,
            result: Err(error),
            ..
        } => {
            tracing::error!(%error, "dictation failed");
            feedback::play(Tone::Error);
            events.dictation(DictationPhase::Failed(error), "")?;
            if let Some(indicator) = indicator {
                indicator.send(DictationIndicatorEvent::JobFailed {
                    job_id: job_id.value(),
                });
            }
        }
        WorkerEvent::Stage { job_id, stage } => {
            if let Some(indicator) = indicator {
                indicator.send(match stage {
                    DictationJobStage::Transcribing => DictationIndicatorEvent::Transcribing {
                        job_id: job_id.value(),
                    },
                });
            }
        }
        WorkerEvent::Cancelled { .. } => {}
        WorkerEvent::ModelFailed(error) => {
            tracing::error!(%error, "dictation failed");
            feedback::play(Tone::Error);
            events.dictation(DictationPhase::Failed(error), "")?;
            if let Some(indicator) = indicator {
                indicator.send(DictationIndicatorEvent::Failed);
            }
        }
        WorkerEvent::Pasted {
            kind,
            result: Ok(text),
        } => {
            let phase = match kind {
                PasteKind::LastTranscript => DictationPhase::Repasted,
            };
            events.dictation(phase, text)?;
        }
        WorkerEvent::Pasted {
            result: Err(error), ..
        } => {
            feedback::play(Tone::Error);
            events.dictation(DictationPhase::Failed(error), "")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programmatic_meter_reports_rms_and_peak_in_decibels() {
        let level = level_for(&[0.5, -0.5, 0.0, 0.0]);
        assert!((level.rms_db - -9.0309).abs() < 0.001);
        assert!((level.peak_db - -6.0206).abs() < 0.001);
        assert_eq!(level_for(&[]).rms_db, -120.0);
    }

    #[test]
    fn programmatic_lease_expires_at_its_deadline() {
        let now = Instant::now();
        let deadline = now + PROGRAMMATIC_LEASE;
        assert!(!programmatic_lease_expired(
            deadline,
            deadline - Duration::from_millis(1)
        ));
        assert!(programmatic_lease_expired(deadline, deadline));
    }

    #[test]
    fn programmatic_completion_cache_is_bounded() {
        let mut completions = VecDeque::new();
        for id in 0..PROGRAMMATIC_COMPLETION_LIMIT as u64 + 2 {
            remember_programmatic_completion(
                &mut completions,
                ProgrammaticCompletion {
                    id,
                    owner_token: format!("owner-{id}"),
                    outcome: ProgrammaticTerminalOutcome::Cancelled,
                },
            );
        }
        assert_eq!(completions.len(), PROGRAMMATIC_COMPLETION_LIMIT);
        assert_eq!(completions.front().map(|completion| completion.id), Some(2));
    }
}
