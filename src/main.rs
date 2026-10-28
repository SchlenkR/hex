#[cfg(target_os = "macos")]
mod accessibility;
mod app_paths;
#[cfg(target_os = "macos")]
mod app_settings;
#[cfg(target_os = "macos")]
mod app_shell;
#[cfg(target_os = "macos")]
mod app_window;
#[cfg(target_os = "macos")]
mod apple_speech;
#[cfg(target_os = "macos")]
mod application_catalog;
mod audio;
#[cfg(target_os = "macos")]
mod config;
#[cfg(target_os = "macos")]
mod context;
#[cfg(target_os = "macos")]
mod desktop_activity;
#[cfg(target_os = "macos")]
mod desktop_host;
#[cfg(target_os = "macos")]
mod desktop_transcription_picker;
#[cfg(target_os = "macos")]
mod desktop_ui;
mod dictation;
#[cfg(target_os = "macos")]
mod dictation_audio;
#[cfg(target_os = "macos")]
mod dictation_diagnostics;
#[cfg(target_os = "macos")]
mod dictation_indicator;
#[cfg(target_os = "macos")]
pub mod dictation_processor;
mod events;
#[cfg(target_os = "macos")]
mod feedback;
mod history;
mod instance;
#[cfg(target_os = "macos")]
mod keyboard;
#[cfg(target_os = "macos")]
mod local_api;
#[cfg(target_os = "macos")]
mod login_item;
#[cfg(target_os = "macos")]
mod onboarding;
#[cfg(target_os = "macos")]
mod parakeet;
#[cfg(target_os = "macos")]
mod paste;
#[cfg(target_os = "macos")]
mod permission_guide;
#[cfg(target_os = "macos")]
mod recognition;
#[cfg(target_os = "macos")]
mod recording_environment;
#[cfg(target_os = "macos")]
mod sparkle;
#[cfg(target_os = "macos")]
mod status_item;
#[cfg(target_os = "macos")]
mod suppression;
#[cfg(target_os = "macos")]
mod system_load;
#[cfg(target_os = "macos")]
mod text_input;
#[cfg(target_os = "macos")]
mod text_replacements;
#[cfg(target_os = "macos")]
mod transcription;
#[cfg(target_os = "macos")]
mod transcription_benchmark;
mod transcription_models;
#[cfg(target_os = "macos")]
mod transcription_preparation;
#[cfg(target_os = "macos")]
mod transcription_service;

#[cfg(target_os = "macos")]
use std::fs::{self, OpenOptions};
#[cfg(target_os = "macos")]
use std::io::{Read, Write};
#[cfg(target_os = "macos")]
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "macos")]
use std::sync::atomic::Ordering;

#[cfg(target_os = "macos")]
use clap::{Parser, Subcommand, ValueEnum};
use color_eyre::Result;
#[cfg(target_os = "macos")]
use color_eyre::eyre::eyre;
#[cfg(target_os = "macos")]
use tracing_subscriber::fmt::writer::MakeWriterExt;

#[cfg(target_os = "macos")]
#[derive(Parser)]
#[command(version, about = "Local, observable voice control")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "macos")]
#[derive(Subcommand)]
enum Command {
    /// Run the GPUI desktop app with local dictation.
    App {
        /// Override the configured microphone preference order.
        #[arg(long)]
        device: Option<String>,
        /// Preview the dictation HUD without starting recognition.
        #[arg(long)]
        preview_dictation: bool,
    },
    /// Launch an isolated, deterministic UI preview without app services.
    Preview {
        /// Production UI surface to open.
        #[arg(value_enum)]
        target: AppPreviewTarget,
        /// Language selected in the transcription picker.
        #[arg(long, default_value = "en")]
        language: String,
        /// Deterministic model installation state shown by the picker.
        #[arg(long, value_enum, default_value = "actual")]
        model_state: AppPreviewModelState,
        /// Select the Global row in the representative Modes preview.
        #[arg(long)]
        select_global_mode: bool,
        /// Show missing post-onboarding permissions in Settings.
        #[arg(long)]
        permissions_missing: bool,
        /// Show the selected dictation model as missing without changing local files.
        #[arg(long)]
        model_missing: bool,
        /// Open the explicit history retention choices in the History preview.
        #[arg(long)]
        open_history_retention: bool,
        /// Show the sidebar update action without starting the updater.
        #[arg(long)]
        update_available: bool,
    },
    /// Listen and transcribe until interrupted.
    Listen {
        /// Override the configured microphone preference order.
        #[arg(long)]
        device: Option<String>,
    },
    /// Run the headless local API service.
    #[command(hide = true)]
    Service {
        /// Run as a direct child whose stdin is owned by the host application.
        #[arg(long)]
        embedded: bool,
    },
    /// Measure the local transcription runtime against a fixed WAV corpus.
    #[command(hide = true)]
    BenchmarkTranscription {
        /// JSON manifest containing audio paths and reference transcripts.
        manifest: PathBuf,
        /// Runtime to measure.
        #[arg(long, value_enum, default_value = "transcribe-cpp")]
        backend: TranscriptionBenchmarkBackend,
        /// Override the default GGUF model path for the transcribe.cpp backend.
        #[arg(long)]
        model: Option<PathBuf>,
        /// Full-corpus passes discarded before measurement.
        #[arg(long, default_value_t = 1)]
        warmups: usize,
        /// Full-corpus measured passes.
        #[arg(long, default_value_t = 7)]
        runs: usize,
    },
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, ValueEnum)]
enum TranscriptionBenchmarkBackend {
    Onnx,
    TranscribeCpp,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, ValueEnum)]
enum AppPreviewTarget {
    DictationHud,
    HudLab,
    Onboarding,
    Settings,
    Modes,
    Replacements,
    Activity,
    History,
    TranscriptionPicker,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, ValueEnum)]
enum AppPreviewModelState {
    Actual,
    Installed,
    Missing,
    Downloading,
    Error,
}

#[cfg(target_os = "macos")]
fn main() -> Result<()> {
    color_eyre::install()?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let log_dir = app_paths::logs_dir()?;
    fs::create_dir_all(&log_dir)?;
    let process_log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("process.log"))?;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("voice_control=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr.and(Mutex::new(process_log)))
        .init();
    ctrlc::set_handler(|| SHUTDOWN.store(true, Ordering::Relaxed))?;

    let event_path = log_dir.join("live.ndjson");
    let cli = Cli::parse();
    let executable_name = std::env::current_exe()
        .ok()
        .and_then(|path| path.file_stem().map(ToOwned::to_owned));
    let bundled_watcher = is_bundled_app_executable(executable_name.as_deref());
    let bundled_service = executable_name.as_deref() == Some(std::ffi::OsStr::new("hex-service"));
    let command = cli.command.unwrap_or({
        if bundled_watcher {
            Command::App {
                device: None,
                preview_dictation: false,
            }
        } else if bundled_service {
            Command::Service { embedded: false }
        } else {
            Command::Listen { device: None }
        }
    });
    match command {
        Command::App {
            device,
            preview_dictation,
        } => {
            let _instance = instance::acquire("listener")?;
            app_shell::run(
                &SHUTDOWN,
                (!preview_dictation).then_some(app_shell::ListenerConfig {
                    project_root: root,
                    event_path,
                    device,
                }),
                preview_dictation,
            )
        }
        Command::Preview {
            target,
            language,
            model_state,
            select_global_mode,
            permissions_missing,
            model_missing,
            open_history_retention,
            update_available,
        } => {
            if matches!(target, AppPreviewTarget::DictationHud) {
                return app_shell::run(&SHUTDOWN, None, true);
            }
            if !transcription_models::LANGUAGES
                .iter()
                .any(|(code, _)| *code == language)
            {
                return Err(eyre!("unsupported preview language: {language}"));
            }
            let pane = match target {
                AppPreviewTarget::HudLab => app_window::PreviewPane::HudLab,
                AppPreviewTarget::Onboarding
                | AppPreviewTarget::Settings
                | AppPreviewTarget::TranscriptionPicker => app_window::PreviewPane::Settings,
                AppPreviewTarget::Modes => app_window::PreviewPane::Modes,
                AppPreviewTarget::Replacements => app_window::PreviewPane::Replacements,
                AppPreviewTarget::Activity => app_window::PreviewPane::Activity,
                AppPreviewTarget::History => app_window::PreviewPane::History,
                AppPreviewTarget::DictationHud => unreachable!(),
            };
            let model_state = match model_state {
                AppPreviewModelState::Actual => app_window::PreviewModelState::Actual,
                AppPreviewModelState::Installed => app_window::PreviewModelState::Installed,
                AppPreviewModelState::Missing => app_window::PreviewModelState::Missing,
                AppPreviewModelState::Downloading => app_window::PreviewModelState::Downloading,
                AppPreviewModelState::Error => app_window::PreviewModelState::Error,
            };
            app_shell::preview_shell(
                &SHUTDOWN,
                app_window::AppWindowPreview {
                    pane,
                    transcription_picker: matches!(target, AppPreviewTarget::TranscriptionPicker)
                        .then_some((language, model_state)),
                    onboarding: matches!(target, AppPreviewTarget::Onboarding),
                    select_global_mode,
                    permissions_missing,
                    model_missing,
                    open_history_retention,
                    update_available,
                },
            )
        }
        Command::Listen { device } => {
            let _instance = instance::acquire("listener")?;
            let settings = app_settings::AppSettings::load()?;
            let events = events::EventLog::create(&event_path)?;
            let history = match history::History::open_default(settings.history_retention) {
                Ok(history) => Some(history),
                Err(error) => {
                    tracing::warn!(%error, "dictation history is unavailable");
                    None
                }
            };
            recognition::listen(events, device.as_deref(), &SHUTDOWN, None, history, None)
        }
        Command::Service { embedded } => {
            if !embedded {
                app_settings::AppSettings::load()?;
            }
            let service_event_path = if embedded {
                log_dir.join(format!("embedded-{}.ndjson", std::process::id()))
            } else {
                event_path
            };
            let events = events::EventLog::create(&service_event_path)?;
            let local_api = if embedded {
                let api = local_api::LocalApi::start_embedded(events)?;
                std::thread::Builder::new()
                    .name("embedded-host-lease".into())
                    .spawn(|| {
                        let mut stdin = std::io::stdin().lock();
                        let mut buffer = [0_u8; 1];
                        loop {
                            match stdin.read(&mut buffer) {
                                Ok(0) => {
                                    SHUTDOWN.store(true, Ordering::Release);
                                    std::thread::sleep(std::time::Duration::from_secs(5));
                                    tracing::warn!(
                                        "forcing embedded service shutdown after host lease closed"
                                    );
                                    std::process::exit(0);
                                }
                                Ok(_) => {}
                                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                                Err(error) => {
                                    tracing::warn!(%error, "embedded host lease failed");
                                    SHUTDOWN.store(true, Ordering::Release);
                                    break;
                                }
                            }
                        }
                    })?;
                let mut stdout = std::io::stdout().lock();
                serde_json::to_writer(&mut stdout, &api.embedded_endpoint())?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
                drop(stdout);
                api
            } else {
                local_api::LocalApi::start(events)?
            };
            while !SHUTDOWN.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            drop(local_api);
            if embedded
                && let Err(error) = fs::remove_file(&service_event_path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(%error, "could not remove embedded service event log");
            }
            Ok(())
        }
        Command::BenchmarkTranscription {
            manifest,
            backend,
            model,
            warmups,
            runs,
        } => {
            let backend = match backend {
                TranscriptionBenchmarkBackend::Onnx => transcription_benchmark::Backend::Onnx,
                TranscriptionBenchmarkBackend::TranscribeCpp => {
                    transcription_benchmark::Backend::TranscribeCpp {
                        model: match model {
                            Some(model) => model,
                            None => parakeet::default_model_path()?,
                        },
                    }
                }
            };
            transcription_benchmark::run(&manifest, warmups, runs, backend)
        }
    }
}

#[cfg(target_os = "macos")]
fn is_bundled_app_executable(name: Option<&std::ffi::OsStr>) -> bool {
    name.is_some_and(|name| name == "voice-control-watch" || name == "hex")
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{Cli, Command, is_bundled_app_executable};
    use clap::Parser;

    #[test]
    fn available_update_requires_an_explicit_preview_flag() {
        for (args, expected) in [
            (vec!["hex", "preview", "settings"], false),
            (
                vec!["hex", "preview", "settings", "--update-available"],
                true,
            ),
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            let Some(Command::Preview {
                update_available, ..
            }) = cli.command
            else {
                panic!("expected preview command");
            };
            assert_eq!(update_available, expected);
        }
        assert!(Cli::try_parse_from(["hex", "app", "--update-available"]).is_err());
    }

    #[test]
    fn both_packaged_executable_names_launch_the_app() {
        assert!(is_bundled_app_executable(Some(
            "voice-control-watch".as_ref()
        )));
        assert!(is_bundled_app_executable(Some("hex".as_ref())));
        assert!(!is_bundled_app_executable(Some("voice-control".as_ref())));
        assert!(!is_bundled_app_executable(Some("hex-service".as_ref())));
    }
}
