use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use color_eyre::eyre::{Result, eyre};
use gpui::{
    App, Application, KeyBinding, Menu, MenuItem, Render, SystemMenuType, Timer, WindowHandle,
};

use crate::app_settings::AppSettings;
use crate::app_window::AppWindow;
use crate::dictation_indicator::DictationIndicatorEvent;
use crate::dictation_indicator::{self, DictationIndicatorUi};
use crate::recognition;
use crate::transcription_preparation::TranscriptionPreparation;

struct ShellUi {
    app_window: Rc<RefCell<Option<WindowHandle<AppWindow>>>>,
    event_path: PathBuf,
    indicator: crate::dictation_indicator::DictationIndicatorSender,
    recognition_start: Option<SyncSender<()>>,
    history: Option<crate::history::History>,
    recognition_controls: SyncSender<crate::recognition::RecognitionControl>,
    status_actions: Option<Receiver<crate::status_item::StatusItemAction>>,
    transcription_preparation: TranscriptionPreparation,
    model_menu_settings: AppSettings,
}

struct RuntimeWorkers {
    local_api: Option<crate::local_api::LocalApi>,
    recognition: Option<thread::JoinHandle<()>>,
}

impl RuntimeWorkers {
    fn join(mut self) -> Result<()> {
        if let Some(mut local_api) = self.local_api.take() {
            local_api.shutdown();
        }
        if let Some(worker) = self.recognition {
            worker
                .join()
                .map_err(|_| eyre!("recognition worker panicked during shutdown"))?;
        }
        Ok(())
    }

    fn join_and_log(self) {
        if let Err(error) = self.join() {
            tracing::error!(%error, "runtime worker shutdown failed");
        }
    }
}

pub struct ListenerConfig {
    pub project_root: PathBuf,
    pub event_path: PathBuf,
    pub device: Option<String>,
}

pub fn run(
    shutdown: &'static AtomicBool,
    listener: Option<ListenerConfig>,
    dictation_preview: bool,
) -> Result<()> {
    run_with_shell_preview(shutdown, listener, dictation_preview, None)
}

pub fn preview_shell(
    shutdown: &'static AtomicBool,
    preview: crate::app_window::AppWindowPreview,
) -> Result<()> {
    run_with_shell_preview(shutdown, None, false, Some(preview))
}

fn should_open_app_on_launch(
    show_dock_icon: bool,
    setup_ready: bool,
    onboarding_completed: bool,
    status_item_available: bool,
) -> bool {
    show_dock_icon || !setup_ready || !onboarding_completed || !status_item_available
}

fn run_with_shell_preview(
    shutdown: &'static AtomicBool,
    listener: Option<ListenerConfig>,
    dictation_preview: bool,
    shell_preview: Option<crate::app_window::AppWindowPreview>,
) -> Result<()> {
    if objc2::MainThreadMarker::new().is_none() {
        return Err(eyre!("desktop startup requires the main thread"));
    }
    shutdown.store(false, Ordering::Relaxed);
    crate::keyboard::initialize_layout()?;
    let settings = if shell_preview.is_some() {
        AppSettings::default()
    } else {
        AppSettings::load()?
    };
    let show_dock_icon = settings.show_dock_icon;
    let setup_ready = crate::onboarding::status(&settings.transcription).ready();
    let onboarding_completed = shell_preview.is_some() || crate::onboarding::completion_recorded();
    let history = if listener.is_some() {
        match crate::history::History::open_default(settings.history_retention) {
            Ok(history) => Some(history),
            Err(error) => {
                tracing::warn!(%error, "dictation history is unavailable");
                None
            }
        }
    } else {
        None
    };
    let app_event_path = listener
        .as_ref()
        .map(|listener| listener.event_path.clone());
    let event_log = app_event_path
        .as_ref()
        .map(|path| crate::events::EventLog::create(path))
        .transpose()?;
    let (recognition_control_sender, recognition_control_receiver) = mpsc::sync_channel(8);
    let local_api = event_log
        .as_ref()
        .map(|events| {
            crate::local_api::LocalApi::start_with_dictation(
                events.clone(),
                recognition_control_sender.clone(),
            )
        })
        .transpose()?;
    let project_root = listener
        .as_ref()
        .map(|listener| listener.project_root.clone())
        .unwrap_or(std::env::current_dir()?);
    let shell_event_path = app_event_path
        .clone()
        .unwrap_or_else(|| project_root.join("logs/live.ndjson"));
    let (indicator_sender, indicator_receiver) = dictation_indicator::channel();
    if dictation_preview {
        let preview_sender = indicator_sender.clone();
        thread::spawn(move || {
            loop {
                preview_sender.send(DictationIndicatorEvent::Started);
                thread::sleep(Duration::from_millis(450));
                let started = Instant::now();
                while started.elapsed() < Duration::from_secs(5) {
                    let wave = (started.elapsed().as_secs_f32() * 5.0).sin() * 0.5 + 0.5;
                    preview_sender.send(DictationIndicatorEvent::Meter {
                        average: 0.025 + wave * 0.09,
                        peak: 0.15 + wave * 0.55,
                    });
                    thread::sleep(Duration::from_millis(20));
                }
                preview_sender.send(DictationIndicatorEvent::Submitted { job_id: 0 });
                preview_sender.send(DictationIndicatorEvent::Transcribing { job_id: 0 });
                thread::sleep(Duration::from_millis(700));
                preview_sender.send(DictationIndicatorEvent::Started);
                thread::sleep(Duration::from_millis(500));
                preview_sender.send(DictationIndicatorEvent::Processing { job_id: 0 });
                thread::sleep(Duration::from_millis(800));
                preview_sender.send(DictationIndicatorEvent::Submitted { job_id: 1 });
                preview_sender.send(DictationIndicatorEvent::Transcribing { job_id: 1 });
                thread::sleep(Duration::from_millis(700));
                preview_sender.send(DictationIndicatorEvent::JobCompleted { job_id: 0 });
                thread::sleep(Duration::from_millis(700));
                preview_sender.send(DictationIndicatorEvent::Processing { job_id: 1 });
                thread::sleep(Duration::from_secs(1));
                preview_sender.send(DictationIndicatorEvent::JobCompleted { job_id: 1 });
                thread::sleep(Duration::from_secs(1));
            }
        });
    }

    let (recognition_start_sender, recognition_start_receiver) = if listener.is_some() {
        let (sender, receiver) = mpsc::sync_channel(1);
        (Some(sender), Some(receiver))
    } else {
        (None, None)
    };
    let recognition_worker = listener.map(|listener| {
        let recognition_events = event_log.clone().expect("listener event log must exist");
        let failure_indicator = indicator_sender.clone();
        let recognition_indicator = indicator_sender.clone();
        let recognition_history = history.clone();
        let start = recognition_start_receiver.expect("listener start receiver must exist");
        thread::spawn(move || {
            if !setup_ready {
                loop {
                    if shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                    match start.recv_timeout(Duration::from_millis(100)) {
                        Ok(()) => break,
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            }
            if let Err(error) = recognition::listen(
                recognition_events,
                listener.device.as_deref(),
                shutdown,
                Some(recognition_indicator),
                recognition_history,
                Some(recognition_control_receiver),
            ) {
                tracing::error!(%error, "desktop recognition stopped");
                failure_indicator.send(crate::dictation_indicator::DictationIndicatorEvent::Failed);
            }
        })
    });
    let runtime_workers = Rc::new(RefCell::new(Some(RuntimeWorkers {
        local_api,
        recognition: recognition_worker,
    })));

    let app_window = Rc::new(RefCell::new(None::<WindowHandle<AppWindow>>));
    let reopen_window = app_window.clone();
    let reopen_event_path = shell_event_path.clone();
    let reopen_recognition_start = recognition_start_sender.clone();
    let reopen_preview = shell_preview.clone();
    let reopen_history = history.clone();
    let reopen_indicator = indicator_sender.clone();
    let indicator_enabled = shell_preview
        .as_ref()
        .is_none_or(|preview| matches!(preview.pane, crate::app_window::PreviewPane::HudLab));
    let application = Application::new();
    application.on_reopen(move |cx| {
        let result = match reopen_preview.clone() {
            Some(preview) => crate::app_window::open_preview(
                &reopen_window,
                reopen_event_path.clone(),
                reopen_indicator.clone(),
                preview,
                cx,
            ),
            None => crate::app_window::open_or_focus(
                &reopen_window,
                reopen_event_path.clone(),
                reopen_indicator.clone(),
                reopen_recognition_start.clone(),
                reopen_history.clone(),
                cx,
            ),
        };
        if let Err(error) = result {
            tracing::error!(%error, "could not open HEX");
        }
    });
    let application_runtime_workers = runtime_workers.clone();
    application.run(move |cx| {
        let transcription_preparation = cx.default_global::<TranscriptionPreparation>().clone();
        let close_app_window = app_window.clone();
        cx.bind_keys([
            KeyBinding::new("cmd-w", crate::app_window::CloseWindow, None),
            KeyBinding::new("cmd-q", crate::app_window::QuitApplication, None),
            KeyBinding::new("cmd-h", crate::app_window::HideApplication, None),
            KeyBinding::new("cmd-m", crate::app_window::MinimizeWindow, None),
            KeyBinding::new("ctrl-cmd-f", crate::app_window::ToggleFullscreen, None),
            KeyBinding::new("cmd-,", crate::app_window::ShowSettings, None),
            KeyBinding::new("cmd-1", crate::app_window::ShowSettings, None),
            KeyBinding::new("cmd-2", crate::app_window::ShowModes, None),
            KeyBinding::new("cmd-3", crate::app_window::ShowHistory, None),
        ]);
        cx.bind_keys(crate::text_input::key_bindings());
        cx.on_action(move |_: &crate::app_window::CloseWindow, cx| {
            close_window(close_app_window.borrow_mut().take(), cx)
        });
        cx.on_action(|_: &crate::app_window::QuitApplication, cx| cx.quit());
        cx.on_action(|_: &crate::app_window::CheckForUpdates, _| {
            crate::sparkle::check_for_updates()
        });
        cx.on_action(|_: &crate::app_window::HideApplication, _| {
            crate::app_settings::hide_application()
        });
        cx.set_menus(vec![
            Menu {
                name: "HEX".into(),
                items: vec![
                    MenuItem::action("Check for Updates…", crate::app_window::CheckForUpdates),
                    MenuItem::separator(),
                    MenuItem::action("Settings…", crate::app_window::ShowSettings),
                    MenuItem::os_submenu("Services", SystemMenuType::Services),
                    MenuItem::separator(),
                    MenuItem::action("Hide HEX", crate::app_window::HideApplication),
                    MenuItem::separator(),
                    MenuItem::action("Quit HEX", crate::app_window::QuitApplication),
                ],
            },
            Menu {
                name: "File".into(),
                items: vec![MenuItem::action(
                    "Close Window",
                    crate::app_window::CloseWindow,
                )],
            },
            Menu {
                name: "Window".into(),
                items: vec![
                    MenuItem::action("Minimize", crate::app_window::MinimizeWindow),
                    MenuItem::action("Enter Full Screen", crate::app_window::ToggleFullscreen),
                ],
            },
        ]);
        if shell_preview.is_none() {
            crate::sparkle::start();
        }
        let status_actions = if shell_preview.is_none() {
            match crate::status_item::install() {
                Ok(actions) => Some(actions),
                Err(error) => {
                    tracing::error!(%error, "could not install HEX status item");
                    None
                }
            }
        } else {
            None
        };
        crate::status_item::update_transcription(&settings, &transcription_preparation.status());
        crate::app_settings::set_dock_icon_visible(show_dock_icon || status_actions.is_none());
        let show_app_on_launch = should_open_app_on_launch(
            show_dock_icon,
            setup_ready,
            onboarding_completed,
            status_actions.is_some(),
        );
        let open_result = match shell_preview.clone() {
            Some(preview) => Some(crate::app_window::open_preview(
                &app_window,
                shell_event_path.clone(),
                indicator_sender.clone(),
                preview,
                cx,
            )),
            None if show_app_on_launch => app_event_path.map(|event_path| {
                crate::app_window::open_or_focus(
                    &app_window,
                    event_path,
                    indicator_sender.clone(),
                    recognition_start_sender.clone(),
                    history.clone(),
                    cx,
                )
            }),
            None => None,
        };
        if let Some(Err(error)) = open_result {
            tracing::error!(%error, "could not open HEX");
        }
        let quit_shutdown = shutdown;
        let quit_workers = application_runtime_workers.clone();
        let quit_preparation = transcription_preparation.clone();
        cx.on_app_quit(move |_| {
            quit_preparation.cancel();
            quit_shutdown.store(true, Ordering::Relaxed);
            if let Some(workers) = quit_workers.borrow_mut().take() {
                workers.join_and_log();
            }
            async {}
        })
        .detach();

        let ui_history = history.clone();
        cx.spawn(async move |cx| {
            drive_ui(
                indicator_receiver,
                ShellUi {
                    app_window: app_window.clone(),
                    event_path: shell_event_path.clone(),
                    indicator: indicator_sender.clone(),
                    recognition_start: recognition_start_sender.clone(),
                    history: ui_history.clone(),
                    recognition_controls: recognition_control_sender.clone(),
                    status_actions,
                    transcription_preparation,
                    model_menu_settings: settings,
                },
                shutdown,
                indicator_enabled,
                cx,
            )
            .await;
        })
        .detach();
    });
    shutdown.store(true, Ordering::Relaxed);
    if let Some(workers) = runtime_workers.borrow_mut().take() {
        workers.join()?
    }
    Ok(())
}

async fn drive_ui(
    indicator_events: Receiver<crate::dictation_indicator::DictationIndicatorEvent>,
    mut ui: ShellUi,
    shutdown: &AtomicBool,
    indicator_enabled: bool,
    cx: &mut gpui::AsyncApp,
) {
    let mut dictation_indicator = indicator_enabled.then(DictationIndicatorUi::new);
    let mut model_menu_refresh_at = Instant::now();

    loop {
        if shutdown.load(Ordering::Relaxed) {
            ui.transcription_preparation.cancel();
            let _ = cx.update(|cx| cx.quit());
            return;
        }
        while let Ok(event) = indicator_events.try_recv() {
            if let Some(dictation_indicator) = &mut dictation_indicator
                && let Err(error) = cx.update(|cx| dictation_indicator.handle(event, cx))
            {
                tracing::error!(%error, "could not update dictation indicator");
                return;
            }
        }
        if let Some(dictation_indicator) = &mut dictation_indicator {
            let _ = cx.update(|cx| dictation_indicator.follow_pointer(cx));
        }
        if let Some(status_actions) = &ui.status_actions {
            while let Ok(action) = status_actions.try_recv() {
                let result = cx.update(|cx| match action {
                    crate::status_item::StatusItemAction::OpenSettings
                    | crate::status_item::StatusItemAction::OpenModels => {
                        match crate::app_window::open_or_focus(
                            &ui.app_window,
                            ui.event_path.clone(),
                            ui.indicator.clone(),
                            ui.recognition_start.clone(),
                            ui.history.clone(),
                            cx,
                        ) {
                            Ok(handle) => {
                                let _ = handle.update(cx, |window, _, cx| {
                                    if action == crate::status_item::StatusItemAction::OpenModels {
                                        window.show_transcription_models(cx);
                                    } else {
                                        window.show_settings(cx);
                                    }
                                });
                            }
                            Err(error) => {
                                tracing::error!(%error, "could not open HEX from status item");
                            }
                        }
                    }
                    crate::status_item::StatusItemAction::SelectModel(model) => {
                        let (_, current) = crate::app_settings::transcription_selection();
                        ui.model_menu_settings.transcription = current;
                        let selection = crate::app_window::menu_transcription_selection(
                            &ui.app_window,
                            model,
                            &ui.model_menu_settings,
                            cx,
                        );
                        ui.transcription_preparation.start(selection, true);
                        model_menu_refresh_at = Instant::now();
                    }
                    crate::status_item::StatusItemAction::CancelModelPreparation => {
                        ui.transcription_preparation.cancel();
                        model_menu_refresh_at = Instant::now();
                    }
                    crate::status_item::StatusItemAction::PasteLast => {
                        let _ = ui
                            .recognition_controls
                            .try_send(crate::recognition::RecognitionControl::PasteLast);
                    }
                    crate::status_item::StatusItemAction::CheckForUpdates => {
                        crate::sparkle::check_for_updates();
                    }
                    crate::status_item::StatusItemAction::Quit => {
                        ui.transcription_preparation.cancel();
                        cx.quit();
                    }
                });
                if let Err(error) = result {
                    tracing::error!(%error, "could not handle HEX status item action");
                    return;
                }
            }
        }
        // Delivered menu cancellations/new choices win over an uncommitted completion.
        if let Some(result) = ui.transcription_preparation.poll() {
            let result = result.and_then(|selection| {
                cx.update(|cx| {
                    crate::app_window::commit_transcription_selection(&ui.app_window, selection, cx)
                })
                .map_err(|error| error.to_string())?
            });
            match result {
                Ok(settings) => {
                    ui.model_menu_settings = settings;
                    ui.transcription_preparation.set_error(None);
                }
                Err(error) => {
                    tracing::warn!(%error, "could not select transcription model");
                    ui.transcription_preparation.set_error(Some(error));
                }
            }
            model_menu_refresh_at = Instant::now();
        }
        if ui.status_actions.is_some() && Instant::now() >= model_menu_refresh_at {
            let status = ui.transcription_preparation.status();
            let _ = cx.update(|cx| {
                crate::app_window::refresh_transcription_menu(
                    &ui.app_window,
                    &ui.model_menu_settings,
                    &status,
                    cx,
                );
            });
            model_menu_refresh_at = Instant::now() + Duration::from_secs(1);
        }
        Timer::after(Duration::from_millis(16)).await;
    }
}

fn close_window<T: Render + 'static>(window: Option<WindowHandle<T>>, cx: &mut App) {
    if let Some(window) = window {
        window
            .update(cx, |_, window, _| window.remove_window())
            .ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dockless_startup_stays_quiet_only_when_setup_and_menu_bar_are_ready() {
        for setup_ready in [false, true] {
            for onboarding_completed in [false, true] {
                for status_item_available in [false, true] {
                    assert_eq!(
                        should_open_app_on_launch(
                            false,
                            setup_ready,
                            onboarding_completed,
                            status_item_available,
                        ),
                        !(setup_ready && onboarding_completed && status_item_available),
                        "setup_ready={setup_ready}, onboarding_completed={onboarding_completed}, status_item_available={status_item_available}",
                    );
                }
            }
        }
    }

    #[test]
    fn dock_visible_startup_always_opens_the_app() {
        for setup_ready in [false, true] {
            for onboarding_completed in [false, true] {
                for status_item_available in [false, true] {
                    assert!(should_open_app_on_launch(
                        true,
                        setup_ready,
                        onboarding_completed,
                        status_item_available,
                    ));
                }
            }
        }
    }
}
