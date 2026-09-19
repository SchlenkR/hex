use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, Context, Div, ElementId, Entity, FocusHandle, Focusable, FontWeight,
    IntoElement, KeyDownEvent, Modifiers as GpuiModifiers, ModifiersChangedEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, PathPromptOptions, Pixels, Point, Render, SharedString,
    Subscription, Timer, TitlebarOptions, Window, WindowBounds, WindowHandle, WindowOptions,
    actions, deferred, div, img, prelude::*, px, relative, rgb, rgba, size,
};

use crate::app_settings::{
    AppSettings, DictationMode, HotkeyBinding, HotkeyKey, HotkeyModifiers, ModifierSide,
    RecordingAudioBehavior, TextReplacement,
};
use crate::application_catalog::InstalledApplication;
use crate::desktop_activity::DesktopActivity;
use crate::desktop_host::{
    DesktopAction, DesktopCapabilities, DesktopHost, DesktopSnapshot, DesktopTranscriptionSnapshot,
    DesktopUpdateStatus,
};
use crate::desktop_transcription_picker::{
    TranscriptionPickerDelegate, TranscriptionPickerModel, TranscriptionPickerProgress,
    TranscriptionPickerStatus, TranscriptionPickerView,
    render_transcription_picker as render_shared_transcription_picker,
    transcription_selection_is_active,
};
use crate::desktop_ui::{
    ACCENT, CANVAS, CONTROL_HEIGHT, FAINT, LINE, MUTED, NEGATIVE, NavigationIcon,
    PANE_CONTENT_WIDTH, PANE_LIST_WIDTH, SECTION_GAP, SIDEBAR_WIDTH, SURFACE, SURFACE_HOVER,
    SURFACE_SELECTED, TEXT, TEXT_SOFT, compact_button, compact_header_plus_button, compact_panel,
    compact_panel_header, compact_plus_button, compact_section_label, disclosure_button,
    empty_message, error_message, header_button, hotkey_keycaps, listener_status, mix_color,
    navigation_item, pane_body, pane_content, pane_header, pane_header_with_action, section_label,
    segmented_control, segmented_item, settings_copy, settings_panel, settings_row,
    settings_section_label, sidebar_frame, toggle, window_frame,
};
use crate::dictation_indicator::{DictationIndicatorEvent, DictationIndicatorSender, HudTuning};
use crate::events::{
    CommandOutcome, DictationPhase, EventReader, TranscriptPhase, VoiceEvent, VoiceState, now_ms,
};
use crate::history::{History, HistoryEntry, HistoryKind, HistoryRetention};
use crate::login_item::LoginItemStatus;
use crate::onboarding::{
    PermissionAction, PermissionKind, PermissionState, PermissionWarning, SetupStatus,
};
use crate::text_input::{
    Changed as TextChanged, Dismissed as TextDismissed, Navigate as TextNavigate,
    Submitted as TextSubmitted, TextInput,
};
use crate::transcription_models::{
    ModelDefinition, ModelRuntime, TranscriptionModelId, TranscriptionSelection, definition,
    validate,
};
use crate::transcription_preparation::{PreparationStatus, TranscriptionPreparation};

const WINDOW_WIDTH: f32 = 1040.0;
const WINDOW_HEIGHT: f32 = 700.0;
const MINIMUM_WIDTH: f32 = 860.0;
const MINIMUM_HEIGHT: f32 = 560.0;
const HOTKEY_MIN_WIDTH: f32 = 148.0;
const HOTKEY_SIDE_SELECTOR_WIDTH: f32 = 116.0;
const PERMISSION_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const ACTIVITY_LIMIT: usize = 100;

fn sidebar_update_button(
    status: crate::sparkle::UpdateStatus,
    preview: bool,
) -> Option<gpui::Stateful<Div>> {
    (status == crate::sparkle::UpdateStatus::UpdateAvailable).then(|| {
        div()
            .id("check-for-updates-sidebar")
            .h(px(30.0))
            .px_3()
            .flex()
            .items_center()
            .rounded_sm()
            .text_size(px(12.0))
            .flex_none()
            .bg(rgb(ACCENT))
            .text_color(rgb(TEXT))
            .font_weight(FontWeight::SEMIBOLD)
            .cursor_pointer()
            .hover(|button| button.bg(mix_color(rgb(ACCENT), rgb(TEXT), 0.12)))
            .child("Update")
            .on_click(move |_, window, cx| {
                if !preview {
                    window.dispatch_action(Box::new(CheckForUpdates), cx);
                    window.refresh();
                }
            })
    })
}

fn settings_pane(content: Div) -> AnyElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(pane_header("Settings"))
        .child(
            div()
                .id("settings-scroll")
                .flex_1()
                .overflow_y_scroll()
                .px_8()
                .pt_1()
                .pb_7()
                .child(
                    div().w_full().flex().justify_center().child(
                        content
                            .w_full()
                            .max_w(px(PANE_CONTENT_WIDTH))
                            .min_w_0()
                            .relative()
                            .debug_selector(|| "settings-content".into()),
                    ),
                ),
        )
        .into_any_element()
}

actions!(
    hex,
    [
        CloseWindow,
        CheckForUpdates,
        HideApplication,
        MinimizeWindow,
        QuitApplication,
        ShowHistory,
        ShowModes,
        ShowSettings,
        ToggleFullscreen,
    ]
);

/// Opens the app shell, or focuses and refreshes an existing shell window.
pub fn open_or_focus(
    app_window: &Rc<RefCell<Option<WindowHandle<AppWindow>>>>,
    event_path: PathBuf,
    indicator: DictationIndicatorSender,
    recognition_start: Option<SyncSender<()>>,
    history: Option<History>,
    cx: &mut App,
) -> gpui::Result<WindowHandle<AppWindow>> {
    if let Some(handle) = app_window.borrow().as_ref().copied()
        && handle
            .update(cx, |this, window, cx| {
                if this.event_reader.path() != event_path {
                    this.event_reader = EventReader::open(event_path.clone());
                }
                this.indicator = indicator.clone();
                this.recognition_start = recognition_start.clone();
                this.history = history.clone();
                this.refresh(cx);
                window.activate_window();
            })
            .is_ok()
    {
        cx.activate(true);
        return Ok(handle);
    }

    open_new(
        app_window,
        event_path,
        indicator,
        recognition_start,
        history,
        None,
        cx,
    )
}

/// Commit through the live editor when present, preserving its unsaved settings.
/// Quiet startup does not require creating or focusing a Settings window.
pub fn commit_transcription_selection(
    app_window: &Rc<RefCell<Option<WindowHandle<AppWindow>>>>,
    selection: TranscriptionSelection,
    cx: &mut App,
) -> Result<AppSettings, String> {
    let handle = app_window.borrow().as_ref().copied();
    if let Some(handle) = handle
        && let Ok(result) = handle.update(cx, |this, _, cx| {
            this.apply_transcription_selection(selection.clone(), cx)?;
            cx.notify();
            Ok(this.settings.clone())
        })
    {
        return result;
    }
    let mut settings = AppSettings::load().map_err(|error| error.to_string())?;
    settings
        .save_transcription(selection)
        .map_err(|error| error.to_string())?;
    Ok(settings)
}

pub fn menu_transcription_selection(
    app_window: &Rc<RefCell<Option<WindowHandle<AppWindow>>>>,
    model: TranscriptionModelId,
    fallback: &AppSettings,
    cx: &mut App,
) -> TranscriptionSelection {
    let handle = app_window.borrow().as_ref().copied();
    handle
        .and_then(|handle| {
            handle
                .update(cx, |this, _, _| {
                    this.settings.transcription_for_model(model)
                })
                .ok()
        })
        .unwrap_or_else(|| fallback.transcription_for_model(model))
}

pub fn refresh_transcription_menu(
    app_window: &Rc<RefCell<Option<WindowHandle<AppWindow>>>>,
    fallback: &AppSettings,
    status: &PreparationStatus,
    cx: &mut App,
) {
    let handle = app_window.borrow().as_ref().copied();
    if handle.is_some_and(|handle| {
        handle
            .update(cx, |this, _, _| {
                crate::status_item::update_transcription(&this.settings, status);
            })
            .is_ok()
    }) {
        return;
    }
    crate::status_item::update_transcription(fallback, status);
}

pub fn open_preview(
    app_window: &Rc<RefCell<Option<WindowHandle<AppWindow>>>>,
    event_path: PathBuf,
    indicator: DictationIndicatorSender,
    preview: AppWindowPreview,
    cx: &mut App,
) -> gpui::Result<WindowHandle<AppWindow>> {
    if let Some(handle) = app_window.borrow().as_ref().copied()
        && handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        cx.activate(true);
        return Ok(handle);
    }
    open_new(
        app_window,
        event_path,
        indicator,
        None,
        None,
        Some(preview),
        cx,
    )
}

fn open_new(
    app_window: &Rc<RefCell<Option<WindowHandle<AppWindow>>>>,
    event_path: PathBuf,
    indicator: DictationIndicatorSender,
    recognition_start: Option<SyncSender<()>>,
    history: Option<History>,
    preview: Option<AppWindowPreview>,
    cx: &mut App,
) -> gpui::Result<WindowHandle<AppWindow>> {
    let preview_mode = preview.is_some();
    let bounds = Bounds::centered(None, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), cx);
    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("HEX".into()),
                appears_transparent: true,
                ..Default::default()
            }),
            is_resizable: !preview_mode,
            is_minimizable: true,
            window_min_size: Some(size(px(MINIMUM_WIDTH), px(MINIMUM_HEIGHT))),
            tabbing_identifier: preview_mode.then(|| "hex-preview".into()),
            ..Default::default()
        },
        |window, cx| {
            cx.new(|cx| {
                AppWindow::new(
                    event_path,
                    indicator,
                    recognition_start,
                    history,
                    preview,
                    window,
                    cx,
                )
            })
        },
    )?;
    *app_window.borrow_mut() = Some(handle);
    cx.activate(true);
    Ok(handle)
}

#[derive(Clone, Copy)]
pub enum PreviewPane {
    HudLab,
    Activity,
    History,
    Modes,
    Replacements,
    Settings,
}

#[derive(Clone, Copy)]
pub enum PreviewModelState {
    Actual,
    Installed,
    Missing,
    Downloading,
    Error,
}

#[derive(Clone)]
pub struct AppWindowPreview {
    pub pane: PreviewPane,
    pub transcription_picker: Option<(String, PreviewModelState)>,
    pub onboarding: bool,
    pub select_global_mode: bool,
    pub permissions_missing: bool,
    pub model_missing: bool,
    pub open_history_retention: bool,
    pub update_available: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Pane {
    HudLab,
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    Activity,
    History,
    Modes,
    #[default]
    Settings,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HudDemoState {
    Recording,
    Transcribing,
    Processing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HudControl {
    LineCount,
    Curvature,
    Speed,
    Blur,
    Glow,
    Depth,
    LightAngle,
    Outline,
}

const HUD_MAX_SPEED: f32 = 24.0;

impl Pane {
    fn on_reopen(self, status: SetupStatus) -> Self {
        if crate::onboarding::permission_warnings(status).is_empty() {
            self
        } else {
            Self::Settings
        }
    }

    const ALL: [Self; 5] = [
        Self::Settings,
        Self::Modes,
        Self::History,
        Self::HudLab,
        Self::Activity,
    ];

    fn all(capabilities: DesktopCapabilities) -> Vec<Self> {
        Self::ALL
            .into_iter()
            .filter(|pane| match pane {
                Self::Settings => true,
                Self::Modes => capabilities.modes,
                Self::History => capabilities.history,
                Self::HudLab => capabilities.hud_lab,
                Self::Activity => capabilities.activity,
            })
            .collect()
    }

    fn label(self) -> &'static str {
        match self {
            Self::HudLab => "HUD Lab",
            Self::Activity => "Activity",
            Self::History => "History",
            Self::Modes => "Modes",
            Self::Settings => "Settings",
        }
    }

    fn icon(self) -> NavigationIcon {
        match self {
            Self::Settings => NavigationIcon::Settings,
            Self::Modes => NavigationIcon::Modes,
            Self::History => NavigationIcon::History,
            Self::Activity => NavigationIcon::Activity,
            Self::HudLab => NavigationIcon::HudLab,
        }
    }

    fn from_preview(pane: PreviewPane) -> Self {
        match pane {
            PreviewPane::HudLab => Self::HudLab,
            PreviewPane::Activity => Self::Activity,
            PreviewPane::History => Self::History,
            PreviewPane::Modes => Self::Modes,
            PreviewPane::Replacements => Self::Modes,
            PreviewPane::Settings => Self::Settings,
        }
    }
}

struct ToggleSpring {
    position: f32,
    velocity: f32,
    target: f32,
    last_frame: Instant,
}

struct ProcessingInput {
    entity: Entity<TextInput>,
    _subscriptions: Vec<Subscription>,
}

struct ModeInputs {
    name: ProcessingInput,
    browser_hosts: ProcessingInput,
    replacements: Vec<ReplacementInputs>,
}

struct ProcessingInputs {
    default_mode: ModeInputs,
    modes: Vec<ModeInputs>,
}

struct ReplacementInputs {
    matched_phrase: ProcessingInput,
    output: ProcessingInput,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ModeSelection {
    Default,
    Custom(usize),
}

enum ApplicationCatalogState {
    Loading,
    Loaded(Vec<InstalledApplication>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SoundSlot {
    Start,
    Stop,
}

impl SoundSlot {
    fn title(self) -> &'static str {
        match self {
            Self::Start => "Start sound",
            Self::Stop => "Stop sound",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Start => "Played when a dictation begins",
            Self::Stop => "Played when a dictation is released",
        }
    }

    fn tone(self) -> crate::feedback::Tone {
        match self {
            Self::Start => crate::feedback::Tone::DictationStart,
            Self::Stop => crate::feedback::Tone::DictationStop,
        }
    }

    fn choice_prefix(self) -> &'static str {
        match self {
            Self::Start => "start-sound-choice",
            Self::Stop => "stop-sound-choice",
        }
    }

    fn element_id(self, suffix: &'static str) -> ElementId {
        let prefix = match self {
            Self::Start => "start-sound",
            Self::Stop => "stop-sound",
        };
        ElementId::Name(format!("{prefix}-{suffix}").into())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HotkeyKind {
    Dictation,
    DictationAlternative(usize),
    PasteLast,
}

impl HotkeyKind {
    fn element_id(self, suffix: &'static str) -> ElementId {
        match self {
            Self::Dictation => ElementId::Name(format!("dictation-hotkey-{suffix}").into()),
            Self::DictationAlternative(index) => {
                ElementId::named_usize(format!("dictation-alternative-hotkey-{suffix}"), index)
            }
            Self::PasteLast => ElementId::Name(format!("paste-last-hotkey-{suffix}").into()),
        }
    }
}

enum HotkeyCaptureState {
    Idle,
    Listening {
        kind: HotkeyKind,
        modifiers: HotkeyModifiers,
        message: Option<&'static str>,
        started_at: Instant,
    },
    Saved {
        kind: HotkeyKind,
        saved_at: Instant,
    },
}

impl HotkeyCaptureState {
    fn is_listening(&self) -> bool {
        matches!(self, Self::Listening { .. })
    }
}

impl ToggleSpring {
    fn new(enabled: bool) -> Self {
        let position = if enabled { 1.0 } else { 0.0 };
        Self::at(position)
    }

    fn at(position: f32) -> Self {
        Self {
            position,
            velocity: 0.0,
            target: position,
            last_frame: Instant::now(),
        }
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.set_target(if enabled { 1.0 } else { 0.0 });
    }

    fn set_target(&mut self, target: f32) {
        if self.target == target {
            return;
        }
        self.target = target;
        self.last_frame = Instant::now();
    }

    fn advance(&mut self, elapsed: Duration) {
        /// Critically damped spring stiffness in rad/s; higher settles faster.
        const STIFFNESS: f32 = 40.0;
        let mut remaining = elapsed.as_secs_f32().min(0.1);
        while remaining > 0.0 {
            let dt = remaining.min(1.0 / 240.0);
            let acceleration = -STIFFNESS.powi(2) * (self.position - self.target)
                - 2.0 * STIFFNESS * self.velocity;
            self.velocity += acceleration * dt;
            self.position += self.velocity * dt;
            remaining -= dt;
        }
        if self.is_settled() {
            self.position = self.target;
            self.velocity = 0.0;
        }
    }

    fn render_position(&mut self, window: &mut Window) -> f32 {
        let now = Instant::now();
        self.advance(now.duration_since(self.last_frame));
        self.last_frame = now;
        if !self.is_settled() {
            window.request_animation_frame();
        }
        self.position
    }

    fn is_settled(&self) -> bool {
        (self.position - self.target).abs() < 0.001 && self.velocity.abs() < 0.01
    }
}

/// Root GPUI entity for the production app shell.
pub struct AppWindow {
    preview: bool,
    pane: Pane,
    event_reader: EventReader,
    activity: DesktopActivity,
    indicator: DictationIndicatorSender,
    hud_tuning: HudTuning,
    hud_demo: HudDemoState,
    hud_queued: usize,
    hud_dragging: Option<HudControl>,
    hud_copied: bool,
    recognition_start: Option<SyncSender<()>>,
    setup_status: SetupStatus,
    setup_visible: bool,
    onboarding_completed: bool,
    permission_refresh_at: Instant,
    settings: AppSettings,
    settings_load_error: Option<String>,
    microphone_devices: Vec<String>,
    microphone_picker_open: bool,
    microphone_picker_error: Option<String>,
    sound_picker_open: Option<SoundSlot>,
    launch_at_login_status: LoginItemStatus,
    launch_at_login_error: Option<String>,
    launch_at_login_toggle: ToggleSpring,
    update_status: crate::sparkle::UpdateStatus,
    update_status_changed_at: Instant,
    release_microphone_toggle: ToggleSpring,
    double_tap_toggle: ToggleSpring,
    double_tap_only_visibility: ToggleSpring,
    dock_icon_toggle: ToggleSpring,
    sound_volume_spring: ToggleSpring,
    recording_audio_spring: ToggleSpring,
    transcription_hints: ProcessingInput,
    transcription_picker_language: Option<String>,
    transcription_picker_error: Option<String>,
    transcription_downloading: Option<TranscriptionModelId>,
    transcription_preparation: TranscriptionPreparation,
    transcription_downloaded_bytes: u64,
    transcription_activation_started: Option<Instant>,
    transcription_preview_installed: Option<bool>,
    processing_inputs: ProcessingInputs,
    selected_mode: ModeSelection,
    application_catalog: ApplicationCatalogState,
    application_catalog_receiver: Option<Receiver<Vec<InstalledApplication>>>,
    application_search: ProcessingInput,
    application_picker_open: bool,
    application_picker_error: Option<String>,
    application_picker_highlight: usize,
    mode_delete_armed: bool,
    mode_context_menu: Option<Point<Pixels>>,
    settings_save_generation: u64,
    settings_dirty: bool,
    hotkey_capture: HotkeyCaptureState,
    hotkey_capture_animation: ToggleSpring,
    hotkey_width_spring: ToggleSpring,
    hotkey_side_animations: [ToggleSpring; 2],
    hotkey_side_selection_springs: [ToggleSpring; 2],
    window_focus: FocusHandle,
    hotkey_focus: FocusHandle,
    _hotkey_blur_subscription: Subscription,
    _activation_subscription: Subscription,
    events: Vec<VoiceEvent>,
    current_context: (Option<String>, Option<String>),
    selected_event: Option<usize>,
    history: Option<History>,
    history_search: ProcessingInput,
    history_entries: Vec<HistoryEntry>,
    selected_history: Option<u64>,
    history_error: Option<String>,
    history_clear_armed: bool,
    history_retention_open: bool,
    history_copied: Option<u64>,
}

impl AppWindow {
    #[allow(clippy::too_many_arguments)]
    fn new(
        event_path: PathBuf,
        indicator: DictationIndicatorSender,
        recognition_start: Option<SyncSender<()>>,
        history: Option<History>,
        preview: Option<AppWindowPreview>,
        native_window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.spawn(async move |window, cx| {
            loop {
                Timer::after(Duration::from_millis(500)).await;
                if window
                    .update(cx, |window, cx| {
                        let application_catalog_changed = window.poll_application_catalog();
                        let transcription_changed = window.poll_transcription_download();
                        let setup_changed = window.poll_setup(false);
                        let login_item_changed = window.poll_login_item();
                        let history_changed = window.poll_history(cx);
                        let update_status = if window.preview {
                            window.update_status
                        } else {
                            crate::sparkle::status()
                        };
                        let update_status_changed = window.update_status != update_status;
                        if update_status_changed {
                            window.update_status = update_status;
                            window.update_status_changed_at = Instant::now();
                        }
                        let update_confirmation_expired = window.update_status
                            == crate::sparkle::UpdateStatus::UpToDate
                            && window.update_status_changed_at.elapsed() >= Duration::from_secs(4);
                        if update_confirmation_expired {
                            window.update_status = crate::sparkle::UpdateStatus::Idle;
                            crate::sparkle::clear_confirmation();
                        }
                        if application_catalog_changed
                            || transcription_changed
                            || setup_changed
                            || login_item_changed
                            || history_changed
                            || update_status_changed
                            || update_confirmation_expired
                        {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let preview_mode = preview.is_some();
        let preview_choices = crate::transcription_models::choices_for_runtime;
        let (settings, settings_load_error) = if let Some(preview) = &preview {
            let mut settings = AppSettings::default();
            if let Some((language, _)) = &preview.transcription_picker
                && let Some(choice) = preview_choices(language).first()
            {
                settings.transcription.model = choice.model.id;
                settings.transcription.language = language.clone();
            }
            if matches!(preview.pane, PreviewPane::Modes | PreviewPane::Replacements) {
                settings.dictation_processing.modes.push(DictationMode {
                    name: "Work notes".into(),
                    applications: vec!["Zed".into()],
                    browser_hosts: vec!["github.com".into()],
                    replacements: vec![TextReplacement {
                        matched_phrase: "open code".into(),
                        output: "OpenCode".into(),
                    }],
                });
            }
            (settings, None)
        } else {
            match AppSettings::load() {
                Ok(settings) => (settings, None),
                Err(error) => {
                    tracing::error!(%error, "could not load app settings");
                    (
                        AppSettings::default(),
                        Some(format!("Could not load app settings: {error:#}")),
                    )
                }
            }
        };
        let transcription_hints =
            Self::transcription_hints_input(&settings.transcription.recognition_hints, cx);
        let processing_inputs = Self::processing_inputs(&settings, cx);
        let application_catalog = if preview_mode {
            ApplicationCatalogState::Loaded(Vec::new())
        } else {
            ApplicationCatalogState::Loading
        };
        let application_search = Self::application_search_input(cx);
        let window_focus = cx.focus_handle();
        window_focus.focus(native_window);
        let hotkey_focus = cx.focus_handle();
        let hotkey_blur_subscription = cx.on_blur(&hotkey_focus, native_window, |this, _, cx| {
            this.cancel_hotkey_capture(cx)
        });
        let activation_subscription =
            cx.observe_window_activation(native_window, |this, window, cx| {
                if window.is_window_active() {
                    this.permission_refresh_at = Instant::now();
                    if this.poll_setup(true) {
                        cx.notify();
                    }
                }
            });
        let preview_picker = preview
            .as_ref()
            .and_then(|preview| preview.transcription_picker.as_ref());
        let preview_model_missing = preview
            .as_ref()
            .is_some_and(|preview| preview.model_missing);
        let preview_model_state = preview_picker.map(|(_, state)| *state);
        let preview_downloading = preview_picker.and_then(|(language, state)| {
            matches!(state, PreviewModelState::Downloading)
                .then(|| preview_choices(language)[0].model.id)
        });
        let selected_model = validate(&settings.transcription).ok();
        let setup_status = if preview.as_ref().is_some_and(|preview| preview.onboarding) {
            SetupStatus {
                microphone: PermissionState::NeedsRequest,
                input_monitoring: PermissionState::NeedsRequest,
                accessibility: PermissionState::NeedsRequest,
                transcription_model: false,
            }
        } else if preview
            .as_ref()
            .is_some_and(|preview| preview.permissions_missing)
        {
            SetupStatus {
                microphone: PermissionState::NeedsRequest,
                input_monitoring: PermissionState::NeedsSettings,
                accessibility: PermissionState::NeedsSettings,
                transcription_model: !preview_model_missing,
            }
        } else if preview_mode {
            SetupStatus {
                microphone: PermissionState::Ready,
                input_monitoring: PermissionState::Ready,
                accessibility: PermissionState::Ready,
                transcription_model: !preview_model_missing,
            }
        } else {
            crate::onboarding::status_with_transcription_model(selected_model.is_some_and(
                |model| {
                    crate::transcription_models::is_installed(
                        model,
                        &settings.transcription.language,
                    )
                },
            ))
        };
        let onboarding_completed = preview_mode || crate::onboarding::completion_recorded();
        let setup_visible = preview.as_ref().is_some_and(|preview| preview.onboarding)
            || !onboarding_completed && !setup_status.ready();
        let microphone_devices = crate::audio::input_device_names().unwrap_or_else(|error| {
            tracing::warn!(%error, "could not list microphones for settings");
            Vec::new()
        });
        let (launch_at_login_status, launch_at_login_error) = if preview_mode {
            (LoginItemStatus::Disabled, None)
        } else {
            match crate::login_item::status() {
                Ok(status) => (status, None),
                Err(error) => (LoginItemStatus::Disabled, Some(error.to_string())),
            }
        };
        let history = if preview_mode {
            preview
                .as_ref()
                .is_some_and(|preview| matches!(preview.pane, PreviewPane::History))
                .then(preview_history)
                .flatten()
        } else {
            history
        };
        let history_search = Self::history_search_input(cx);
        let mut window = Self {
            preview: preview_mode,
            pane: preview
                .as_ref()
                .map_or(Pane::default(), |preview| Pane::from_preview(preview.pane)),
            event_reader: EventReader::open(event_path),
            activity: DesktopActivity::default(),
            indicator,
            hud_tuning: HudTuning::default(),
            hud_demo: HudDemoState::Transcribing,
            hud_queued: 1,
            hud_dragging: None,
            hud_copied: false,
            recognition_start,
            setup_status,
            setup_visible,
            onboarding_completed,
            permission_refresh_at: Instant::now() + PERMISSION_REFRESH_INTERVAL,
            microphone_devices,
            microphone_picker_open: false,
            microphone_picker_error: None,
            sound_picker_open: None,
            launch_at_login_status,
            launch_at_login_error,
            launch_at_login_toggle: ToggleSpring::new(
                launch_at_login_status == LoginItemStatus::Enabled,
            ),
            update_status: if let Some(preview) = &preview {
                if preview.update_available {
                    crate::sparkle::UpdateStatus::UpdateAvailable
                } else {
                    crate::sparkle::UpdateStatus::Idle
                }
            } else {
                crate::sparkle::status()
            },
            update_status_changed_at: Instant::now(),
            release_microphone_toggle: ToggleSpring::new(settings.release_microphone_while_idle),
            double_tap_toggle: ToggleSpring::new(settings.double_tap_lock),
            double_tap_only_visibility: ToggleSpring::new(
                settings.double_tap_lock && settings.dictation_hotkey.key.is_some(),
            ),
            dock_icon_toggle: ToggleSpring::new(settings.show_dock_icon),
            sound_volume_spring: ToggleSpring::at(sound_volume_index(&settings) as f32),
            recording_audio_spring: ToggleSpring::at(recording_audio_index(
                settings.recording_audio_behavior,
            ) as f32),
            transcription_hints,
            transcription_picker_language: preview_picker.map(|(language, _)| language.clone()),
            transcription_picker_error: matches!(
                preview_model_state,
                Some(PreviewModelState::Error)
            )
            .then(|| "Preview download failed while verifying the model checksum.".into()),
            transcription_downloading: preview_downloading,
            transcription_preparation: if preview.is_some() {
                TranscriptionPreparation::default()
            } else {
                cx.default_global::<TranscriptionPreparation>().clone()
            },
            transcription_downloaded_bytes: preview_downloading
                .and_then(|model| definition(model).download_bytes())
                .map_or(0, |bytes| bytes * 37 / 100),
            transcription_activation_started: None,
            transcription_preview_installed: match preview_model_state {
                Some(PreviewModelState::Installed) => Some(true),
                Some(
                    PreviewModelState::Missing
                    | PreviewModelState::Downloading
                    | PreviewModelState::Error,
                ) => Some(false),
                Some(PreviewModelState::Actual) | None => preview_model_missing.then_some(false),
            },
            processing_inputs,
            selected_mode: if preview
                .as_ref()
                .is_some_and(|preview| preview.select_global_mode)
            {
                ModeSelection::Default
            } else if preview.as_ref().is_some_and(|preview| {
                matches!(preview.pane, PreviewPane::Modes | PreviewPane::Replacements)
            }) {
                ModeSelection::Custom(0)
            } else {
                ModeSelection::Default
            },
            application_catalog,
            application_catalog_receiver: None,
            application_search,
            application_picker_open: false,
            application_picker_error: None,
            application_picker_highlight: 0,
            mode_delete_armed: false,
            mode_context_menu: None,
            settings_save_generation: 0,
            settings_dirty: false,
            hotkey_capture: HotkeyCaptureState::Idle,
            hotkey_capture_animation: ToggleSpring::new(false),
            hotkey_width_spring: ToggleSpring::at(HOTKEY_MIN_WIDTH),
            hotkey_side_animations: [
                ToggleSpring::new(standalone_modifier_side(&settings.dictation_hotkey).is_some()),
                ToggleSpring::new(
                    settings
                        .paste_last_hotkey
                        .as_ref()
                        .and_then(standalone_modifier_side)
                        .is_some(),
                ),
            ],
            hotkey_side_selection_springs: [
                ToggleSpring::at(hotkey_side_index(
                    standalone_modifier_side(&settings.dictation_hotkey)
                        .unwrap_or(ModifierSide::Either),
                ) as f32),
                ToggleSpring::at(hotkey_side_index(
                    settings
                        .paste_last_hotkey
                        .as_ref()
                        .and_then(standalone_modifier_side)
                        .unwrap_or(ModifierSide::Either),
                ) as f32),
            ],
            window_focus,
            hotkey_focus,
            _hotkey_blur_subscription: hotkey_blur_subscription,
            _activation_subscription: activation_subscription,
            settings,
            settings_load_error,
            events: Vec::new(),
            current_context: (None, None),
            selected_event: None,
            history,
            history_search,
            history_entries: Vec::new(),
            selected_history: None,
            history_error: None,
            history_clear_armed: false,
            history_retention_open: preview
                .as_ref()
                .is_some_and(|preview| preview.open_history_retention),
            history_copied: None,
        };
        if !window.preview {
            window.reload_events();
        }
        window.reload_history(cx);
        if window.preview {
            window.selected_history = window.history_entries.first().map(|entry| entry.id);
        }
        if window.pane == Pane::Modes {
            window.ensure_application_catalog_load();
        }
        if window.pane == Pane::HudLab {
            window.apply_hud_lab();
        }
        window
    }

    /// Refreshes disk-backed activity data while retaining valid selections.
    /// This is also called whenever an existing shell is reopened.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if !self.preview {
            self.reload_events();
        }
        self.reload_history(cx);
        if self.pane == Pane::Modes {
            self.ensure_application_catalog_load();
        }
        self.permission_refresh_at = Instant::now();
        self.poll_setup(true);
        let pane = self.pane.on_reopen(self.setup_status);
        if pane != self.pane {
            self.select_pane(pane, cx);
        }
        cx.notify();
    }

    fn select_pane(&mut self, pane: Pane, cx: &mut Context<Self>) {
        self.pane = pane;
        self.mode_context_menu = None;
        self.history_retention_open = false;
        match pane {
            Pane::HudLab => self.apply_hud_lab(),
            Pane::Activity => self.reload_events(),
            Pane::History => self.reload_history(cx),
            Pane::Modes => self.ensure_application_catalog_load(),
            Pane::Settings => self.permission_refresh_at = Instant::now(),
        }
        cx.notify();
    }

    pub(crate) fn show_settings(&mut self, cx: &mut Context<Self>) {
        self.select_pane(Pane::Settings, cx);
    }

    pub(crate) fn show_transcription_models(&mut self, cx: &mut Context<Self>) {
        self.show_settings(cx);
        self.transcription_picker_language = Some(self.settings.transcription.language.clone());
        self.poll_transcription_download();
        cx.notify();
    }

    fn set_release_microphone_while_idle(
        &mut self,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> color_eyre::Result<()> {
        let mut candidate = self.settings.clone();
        candidate.release_microphone_while_idle = enabled;
        if !self.preview {
            candidate.save()?;
        }
        self.settings = candidate;
        self.release_microphone_toggle.set_enabled(enabled);
        self.settings_save_generation = self.settings_save_generation.wrapping_add(1);
        self.settings_dirty = false;
        self.settings_load_error = None;
        cx.notify();
        Ok(())
    }

    fn poll_application_catalog(&mut self) -> bool {
        let Some(receiver) = &self.application_catalog_receiver else {
            return false;
        };
        let Ok(result) = receiver.try_recv() else {
            return false;
        };
        let mut applications = result;
        if let ApplicationCatalogState::Loaded(existing) = &self.application_catalog {
            for application in existing.iter().cloned() {
                crate::application_catalog::insert(&mut applications, application);
            }
        }
        self.application_catalog = ApplicationCatalogState::Loaded(applications);
        self.application_catalog_receiver = None;
        true
    }

    fn ensure_application_catalog_load(&mut self) {
        if !application_catalog_load_needed(
            self.preview,
            &self.application_catalog,
            self.application_catalog_receiver.is_some(),
        ) {
            return;
        }
        let (sender, receiver) = sync_channel(1);
        thread::spawn(move || {
            let _ = sender.send(crate::application_catalog::discover());
        });
        self.application_catalog_receiver = Some(receiver);
    }

    fn poll_transcription_download(&mut self) -> bool {
        if self.preview {
            return false;
        }
        let status = self.transcription_preparation.status();
        let changed = self.transcription_downloading != status.model
            || self.transcription_downloaded_bytes != status.downloaded_bytes
            || self.transcription_picker_error != status.error;
        self.transcription_downloading = status.model;
        self.transcription_downloaded_bytes = status.downloaded_bytes;
        self.transcription_activation_started = status.started;
        self.transcription_picker_error = status.error;
        changed || status.model.is_some()
    }

    fn poll_setup(&mut self, force: bool) -> bool {
        if self.preview
            || !force
                && (Instant::now() < self.permission_refresh_at
                    || !self.setup_visible
                        && self.recognition_start.is_none()
                        && self.pane != Pane::Settings
                        && self.setup_status.transcription_model)
        {
            return false;
        }
        self.permission_refresh_at = Instant::now() + PERMISSION_REFRESH_INTERVAL;
        let mut changed = false;
        let transcription_model = validate(&self.settings.transcription).is_ok_and(|model| {
            crate::transcription_models::is_installed(model, &self.settings.transcription.language)
        });
        let status = crate::onboarding::status_with_transcription_model(transcription_model);
        if status != self.setup_status {
            self.setup_status = status;
            changed = true;
        }
        if self.setup_status.ready() {
            if !self.onboarding_completed {
                if let Err(error) = crate::onboarding::record_completion() {
                    tracing::warn!(%error, "could not record onboarding completion");
                }
                self.onboarding_completed = true;
            }
            if let Some(start) = self.recognition_start.take() {
                let _ = start.try_send(());
            }
            if self.setup_visible {
                self.setup_visible = false;
                changed = true;
            }
        }
        changed
    }

    fn poll_login_item(&mut self) -> bool {
        if self.preview {
            return false;
        }
        let Ok(status) = crate::login_item::status() else {
            return false;
        };
        if status == self.launch_at_login_status {
            return false;
        }
        self.launch_at_login_status = status;
        self.launch_at_login_toggle
            .set_enabled(status == LoginItemStatus::Enabled);
        if status == LoginItemStatus::Enabled {
            self.launch_at_login_error = None;
        }
        true
    }

    fn apply_transcription_selection(
        &mut self,
        selection: TranscriptionSelection,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.preview {
            self.settings.remember_transcription(selection);
            self.transcription_preview_installed = Some(true);
            self.setup_status.transcription_model = true;
        } else {
            self.settings
                .save_transcription(selection)
                .map_err(|error| error.to_string())?;
        }
        self.transcription_hints.entity.update(cx, |input, cx| {
            input.set_text(&self.settings.transcription.recognition_hints, cx);
        });
        self.permission_refresh_at = Instant::now();
        self.transcription_picker_language = None;
        self.clear_transcription_error();
        self.settings_load_error = None;
        Ok(())
    }

    fn choose_transcription_model(
        &mut self,
        model: &'static ModelDefinition,
        language: String,
        cx: &mut Context<Self>,
    ) {
        self.cancel_transcription_download();
        let selection = TranscriptionSelection {
            model: model.id,
            language,
            recognition_hints: if model.supports_recognition_hints {
                self.transcription_hints.entity.read(cx).text().to_string()
            } else {
                String::new()
            },
        };
        self.clear_transcription_error();
        if self.preview {
            self.apply_transcription_selection(selection, cx)
                .expect("preview selections do not write settings");
            return;
        }
        if transcription_selection_is_active(
            &self.settings.transcription,
            model,
            &selection.language,
            self.transcription_model_installed(model, &selection.language),
        ) {
            self.transcription_picker_language = None;
            return;
        }
        self.transcription_preparation.start(selection, false);
        self.poll_transcription_download();
    }

    fn cancel_transcription_download(&mut self) {
        self.transcription_preparation.cancel();
        self.transcription_downloading = None;
        self.transcription_downloaded_bytes = 0;
        self.transcription_activation_started = None;
    }

    fn clear_transcription_error(&mut self) {
        self.transcription_preparation.set_error(None);
        self.transcription_picker_error = None;
    }

    fn transcription_model_installed(&self, model: &ModelDefinition, language: &str) -> bool {
        self.transcription_preview_installed
            .unwrap_or_else(|| crate::transcription_models::is_installed(model, language))
    }

    fn reload_events(&mut self) {
        let selected = self
            .selected_event
            .and_then(|index| self.events.get(index))
            .cloned();
        self.activity.refresh(&mut self.event_reader);
        if self.activity.error.is_some() {
            return;
        }
        self.events = self.event_reader.recent(ACTIVITY_LIMIT);
        self.current_context = activity_context(&self.event_reader);
        self.selected_event =
            selected.and_then(|selected| self.events.iter().position(|event| event == &selected));
    }

    fn history_search_input(cx: &mut Context<Self>) -> ProcessingInput {
        let entity = cx.new(|cx| TextInput::picker(cx, "Search history", ""));
        let changed = cx.subscribe(&entity, |this, _, _: &TextChanged, cx| {
            this.reload_history(cx);
            cx.notify();
        });
        ProcessingInput {
            entity,
            _subscriptions: vec![changed],
        }
    }

    /// Refresh the bounded history snapshot for the current search query.
    fn reload_history(&mut self, cx: &App) {
        let Some(history) = &self.history else {
            self.history_entries.clear();
            self.selected_history = None;
            return;
        };
        let query = self.history_search.entity.read(cx).text().to_string();
        self.history_entries = history.search(&query);
        if self
            .selected_history
            .is_some_and(|id| !self.history_entries.iter().any(|entry| entry.id == id))
        {
            self.selected_history = None;
        }
    }

    /// Refresh the visible history while the pane is open so entries recorded
    /// by the recognition side appear without reopening the pane.
    fn poll_history(&mut self, cx: &App) -> bool {
        if self.preview || self.pane != Pane::History || self.history.is_none() {
            return false;
        }
        let previous = std::mem::take(&mut self.history_entries);
        self.reload_history(cx);
        self.history_entries != previous
    }

    fn set_history_retention(&mut self, retention: HistoryRetention, cx: &mut Context<Self>) {
        if self.settings.history_retention == retention {
            return;
        }
        self.settings.history_retention = retention;
        if let Some(history) = &self.history
            && let Err(error) = history.set_retention(retention)
        {
            self.history_error = Some(error.to_string());
        }
        self.save_settings(cx);
        self.reload_history(cx);
        cx.notify();
    }

    fn copy_history_entry(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(entry) = self.history_entries.iter().find(|entry| entry.id == id) else {
            return;
        };
        match arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(entry.final_text.clone()))
        {
            Ok(()) => {
                self.history_copied = Some(id);
                self.history_error = None;
            }
            Err(error) => self.history_error = Some(error.to_string()),
        }
        cx.notify();
    }

    fn delete_history_entry(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(history) = &self.history {
            if let Err(error) = history.delete(id) {
                self.history_error = Some(error.to_string());
            }
            self.reload_history(cx);
        }
        cx.notify();
    }

    fn clear_history(&mut self, cx: &mut Context<Self>) {
        if !self.history_clear_armed {
            self.history_clear_armed = true;
            cx.notify();
            return;
        }
        self.history_clear_armed = false;
        if let Some(history) = &self.history {
            if let Err(error) = history.clear() {
                self.history_error = Some(error.to_string());
            }
            self.reload_history(cx);
        }
        cx.notify();
    }

    fn render_history(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let retention = self.settings.history_retention;
        let search = div().w(px(220.0)).child(self.history_search.entity.clone());
        let retention_control = header_button(format!("Keep: {}", retention.label()))
            .id("history-retention")
            .on_click(cx.listener(|this, _, _, cx| {
                this.history_retention_open = true;
                cx.notify();
            }));
        let retention_control = div().relative().child(retention_control).when(
            self.history_retention_open,
            |control| {
                control.child(deferred(
                    div()
                        .id("history-retention-menu")
                        .absolute()
                        .top(px(36.0))
                        .right_0()
                        .w(px(160.0))
                        .p_1()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(LINE))
                        .bg(rgb(SURFACE))
                        .occlude()
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.history_retention_open = false;
                            cx.notify();
                        }))
                        .children(HistoryRetention::ALL.into_iter().enumerate().map(
                            |(index, choice)| {
                                compact_button(choice.label())
                                    .id(("history-retention-choice", index))
                                    .w_full()
                                    .when(choice == retention, |item| {
                                        item.bg(rgb(SURFACE_SELECTED))
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.history_retention_open = false;
                                        this.set_history_retention(choice, cx);
                                        cx.notify();
                                    }))
                            },
                        )),
                ))
            },
        );
        let clear = header_button(if self.history_clear_armed {
            "Really clear all?"
        } else {
            "Clear all"
        })
        .id("history-clear")
        .when(self.history_clear_armed, |button| {
            button.text_color(rgb(0xff8a80))
        })
        .on_click(cx.listener(|this, _, _, cx| this.clear_history(cx)))
        .into_any_element();
        let header_action = div()
            .flex()
            .items_center()
            .gap_3()
            .child(search)
            .child(retention_control)
            .child(clear)
            .into_any_element();
        let rows: Vec<AnyElement> = self
            .history_entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let id = entry.id;
                let selected = self.selected_history == Some(id);
                let age = event_age(entry.timestamp_ms);
                let preview = entry.final_text.replace('\n', " ");
                let mut meta = entry.kind.label().to_string();
                if let Some(application) = &entry.application {
                    meta.push_str(" · ");
                    meta.push_str(application);
                }
                div()
                    .id(("history-entry", index))
                    .w_full()
                    .px_4()
                    .py_3()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap_4()
                    .border_b_1()
                    .border_color(rgb(LINE))
                    .when(selected, |row| row.bg(rgb(SURFACE_SELECTED)))
                    .hover(|row| row.bg(rgb(SURFACE_HOVER)))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .w_full()
                                    .text_size(px(12.0))
                                    .text_color(rgb(TEXT_SOFT))
                                    .line_height(px(18.0))
                                    .truncate()
                                    .child(preview),
                            )
                            .child(div().text_size(px(10.0)).text_color(rgb(FAINT)).child(meta)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(rgb(FAINT))
                            .child(age),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected_history = Some(id);
                        this.history_clear_armed = false;
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
        let retention_off = retention.is_off();
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(pane_header_with_action("History", Some(header_action)))
            .child(
                pane_body().p_5().child(
                    pane_content()
                        .flex_row()
                        .gap_5()
                        .child(
                            compact_panel()
                                .id("history-list")
                                .w(px(PANE_LIST_WIDTH))
                                .h_full()
                                .flex_none()
                                .overflow_y_scroll()
                                .when(retention_off, |list| {
                                    list.child(empty_message(
                                        "History is off. New dictations are not retained.",
                                    ))
                                })
                                .when(
                                    !retention_off
                                        && self.history_entries.is_empty()
                                        && self.history_error.is_none(),
                                    |list| list.child(empty_message("No dictations retained yet.")),
                                )
                                .when_some(self.history_error.clone(), |list, error| {
                                    list.child(error_message("History could not be loaded.", error))
                                })
                                .child(
                                    div()
                                        .w(px(PANE_LIST_WIDTH - 2.0))
                                        .flex()
                                        .flex_col()
                                        .children(rows),
                                ),
                        )
                        .child(
                            compact_panel()
                                .flex_1()
                                .min_w(px(0.0))
                                .h_full()
                                .flex()
                                .flex_col()
                                .child(self.render_history_detail(cx)),
                        ),
                ),
            )
            .into_any_element()
    }

    fn render_history_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(entry) = self
            .selected_history
            .and_then(|id| self.history_entries.iter().find(|entry| entry.id == id))
        else {
            return detail_placeholder("Select a history entry.");
        };
        let id = entry.id;
        let copied = self.history_copied == Some(id);
        let show_raw = entry.raw_text.trim() != entry.final_text.trim();
        let action_button = |label: &'static str, id_suffix: &'static str| {
            div()
                .id(SharedString::from(format!("history-action-{id_suffix}")))
                .h(px(30.0))
                .px_3()
                .flex()
                .items_center()
                .rounded_sm()
                .bg(rgb(SURFACE))
                .text_size(px(12.0))
                .text_color(rgb(TEXT_SOFT))
                .hover(|button| button.bg(rgb(SURFACE_HOVER)).text_color(rgb(TEXT)))
                .child(label)
        };
        let mut latency = format!(
            "{} ms audio · {} ms inference",
            entry.audio_ms, entry.inference_ms
        );
        if entry.total_ms > 0 {
            latency.push_str(&format!(" · {} ms total", entry.total_ms));
        }
        div()
            .id("history-detail")
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .overflow_y_scroll()
            .px_6()
            .py_6()
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .text_size(px(18.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(entry.kind.label()),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(rgb(FAINT))
                                    .child(event_age(entry.timestamp_ms)),
                            ),
                    )
                    .child(
                        div()
                            .pt_4()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                action_button(if copied { "Copied" } else { "Copy" }, "copy")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.copy_history_entry(id, cx)
                                    })),
                            )
                            .child(action_button("Delete", "delete").on_click(cx.listener(
                                move |this, _, _, cx| this.delete_history_entry(id, cx),
                            ))),
                    )
                    .child(
                        div()
                            .mt_5()
                            .pt_5()
                            .border_t_1()
                            .border_color(rgb(LINE))
                            .child(section_label("Final text"))
                            .child(
                                div()
                                    .pt_3()
                                    .text_size(px(12.0))
                                    .line_height(px(19.0))
                                    .text_color(rgb(TEXT))
                                    .child(entry.final_text.clone()),
                            ),
                    )
                    .when(show_raw, |detail| {
                        detail.child(
                            div()
                                .mt_5()
                                .pt_5()
                                .border_t_1()
                                .border_color(rgb(LINE))
                                .child(section_label("Raw transcript"))
                                .child(
                                    div()
                                        .pt_3()
                                        .text_size(px(12.0))
                                        .line_height(px(19.0))
                                        .text_color(rgb(MUTED))
                                        .child(entry.raw_text.clone()),
                                ),
                        )
                    })
                    .child(
                        div()
                            .mt_5()
                            .pt_5()
                            .border_t_1()
                            .border_color(rgb(LINE))
                            .when_some(entry.application.clone(), |detail, application| {
                                detail.child(detail_row("Application", application))
                            })
                            .when_some(entry.processing.clone(), |detail, processing| {
                                let mut summary = format!(
                                    "{} · {} ms",
                                    processing.profile, processing.latency_ms
                                );
                                if let Some(fallback) = &processing.fallback {
                                    summary.push_str(&format!(" · fell back: {fallback}"));
                                }
                                detail.child(detail_row("Processing", summary))
                            })
                            .child(detail_row("Latency", latency)),
                    ),
            )
            .into_any_element()
    }

    fn render_navigation(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let items = Pane::all(self.capabilities())
            .into_iter()
            .enumerate()
            .map(|(index, pane)| {
                let selected = self.pane == pane;
                navigation_item(pane.icon(), selected)
                    .id(("app-nav", index))
                    .child(pane.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_pane(pane, cx);
                    }))
            });

        sidebar_frame()
            .w(px(SIDEBAR_WIDTH))
            .px(px(14.0))
            .pt(px(52.0))
            .pb_4()
            .flex()
            .flex_col()
            .child(div().flex().flex_col().gap(px(2.0)).children(items))
            .child(div().flex_1())
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .h(px(30.0))
                    .mt_3()
                    .child(
                        div()
                            .flex_none()
                            .pl_2()
                            .text_size(px(11.0))
                            .text_color(rgb(MUTED))
                            .child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
                    )
                    .children(sidebar_update_button(self.update_status, self.preview)),
            )
            .into_any_element()
    }

    fn render_hotkey_control(
        &mut self,
        kind: HotkeyKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let binding = match kind {
            HotkeyKind::Dictation => &self.settings.dictation_hotkey,
            HotkeyKind::DictationAlternative(index) => self
                .settings
                .dictation_hotkey_alternatives
                .get(index)
                .unwrap_or(&self.settings.dictation_hotkey),
            HotkeyKind::PasteLast => self
                .settings
                .paste_last_hotkey
                .as_ref()
                .unwrap_or(&self.settings.dictation_hotkey),
        };
        let binding_keycaps = match kind {
            HotkeyKind::Dictation => self.snapshot().dictation_shortcut,
            HotkeyKind::DictationAlternative(_) if binding.is_empty() => vec!["Not set".into()],
            HotkeyKind::DictationAlternative(_) => binding.keycaps(),
            HotkeyKind::PasteLast => self
                .settings
                .paste_last_hotkey
                .as_ref()
                .map_or_else(|| vec!["Off".into()], HotkeyBinding::keycaps),
        };
        let idle_width = hotkey_idle_width(binding_keycaps.len());
        if matches!(
            self.hotkey_capture,
            HotkeyCaptureState::Saved { saved_at, .. }
                if saved_at.elapsed() >= Duration::from_millis(700)
        ) {
            self.hotkey_capture = HotkeyCaptureState::Idle;
            self.hotkey_capture_animation.set_enabled(false);
            self.hotkey_width_spring.set_target(idle_width);
        } else if matches!(self.hotkey_capture, HotkeyCaptureState::Idle) {
            self.hotkey_width_spring.set_target(idle_width);
        }
        let intensity = self.hotkey_capture_animation.render_position(window);
        let animated_control_width = self.hotkey_width_spring.render_position(window);
        let this_capture = matches!(
            self.hotkey_capture,
            HotkeyCaptureState::Listening { kind: active, .. }
                | HotkeyCaptureState::Saved { kind: active, .. }
                if active == kind
        );
        let control_width = hotkey_control_width(this_capture, idle_width, animated_control_width);
        let capture_active = !matches!(self.hotkey_capture, HotkeyCaptureState::Idle);
        let control_intensity = hotkey_control_intensity(this_capture, intensity);
        let capture_color = if matches!(
            self.hotkey_capture,
            HotkeyCaptureState::Saved { kind: active, .. } if active == kind
        ) {
            rgb(0x1b2420)
        } else {
            rgb(0x251c1b)
        };
        let pulse = match &self.hotkey_capture {
            HotkeyCaptureState::Listening {
                kind: active,
                started_at,
                ..
            } if *active == kind => {
                window.request_animation_frame();
                (started_at.elapsed().as_secs_f32() * 4.5).sin() * 0.5 + 0.5
            }
            HotkeyCaptureState::Saved { kind: active, .. } if *active == kind => {
                window.request_animation_frame();
                1.0
            }
            HotkeyCaptureState::Idle
            | HotkeyCaptureState::Listening { .. }
            | HotkeyCaptureState::Saved { .. } => 0.0,
        };
        let content = match &self.hotkey_capture {
            HotkeyCaptureState::Listening {
                kind: active,
                modifiers,
                message,
                ..
            } if *active == kind => {
                let label = message.unwrap_or(if modifiers.is_empty() {
                    "Press shortcut"
                } else {
                    "Release to save or add key"
                });
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .size(px(5.0 + pulse * 3.0))
                            .ml_2()
                            .rounded_full()
                            .bg(rgb(NEGATIVE))
                            .opacity(0.6 + pulse * 0.4),
                    )
                    .when(!modifiers.is_empty() && message.is_none(), |content| {
                        content.child(hotkey_keycaps(
                            HotkeyBinding {
                                modifiers: *modifiers,
                                key: None,
                            }
                            .keycaps(),
                            0.85,
                        ))
                    })
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(rgb(TEXT_SOFT))
                            .child(label),
                    )
                    .child(
                        div()
                            .id(kind.element_id("cancel-capture"))
                            .h(px(26.0))
                            .ml_1()
                            .px_2()
                            .flex()
                            .items_center()
                            .rounded(px(4.0))
                            .text_size(px(11.0))
                            .text_color(rgb(MUTED))
                            .hover(|button| {
                                button.bg(rgb(SURFACE_HOVER)).text_color(rgb(TEXT_SOFT))
                            })
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.cancel_hotkey_capture(cx);
                            })),
                    )
                    .into_any_element()
            }
            HotkeyCaptureState::Saved { kind: active, .. } if *active == kind => div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_size(px(10.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT_SOFT))
                        .child("Saved"),
                )
                .child(hotkey_keycaps(binding_keycaps.clone(), 1.0))
                .into_any_element(),
            HotkeyCaptureState::Idle
            | HotkeyCaptureState::Listening { .. }
            | HotkeyCaptureState::Saved { .. } => div()
                .flex()
                .items_center()
                .gap_2()
                .child(hotkey_keycaps(binding_keycaps, 1.0))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(TEXT_SOFT))
                        .child("Change shortcut"),
                )
                .into_any_element(),
        };
        div()
            .id(kind.element_id("control"))
            .track_focus(&self.hotkey_focus)
            .w(px(control_width))
            .min_w(px(HOTKEY_MIN_WIDTH))
            .h(px(32.0))
            .px(px(4.0))
            .flex()
            .items_center()
            .overflow_hidden()
            .rounded(px(6.0))
            .border_1()
            .border_color(mix_color(
                rgb(LINE),
                rgb(TEXT_SOFT),
                control_intensity * 0.55,
            ))
            .bg(mix_color(
                rgb(CANVAS),
                capture_color,
                control_intensity * (0.55 + pulse * 0.15),
            ))
            .when(!capture_active, |control| {
                control.hover(|control| control.bg(rgb(SURFACE_HOVER)))
            })
            .child(content)
            .on_click(cx.listener(move |this, _, window, cx| {
                if this.hotkey_capture.is_listening() && this_capture {
                    this.cancel_hotkey_capture(cx);
                } else {
                    this.begin_hotkey_capture(kind, window, cx);
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                this.capture_hotkey_key(event, cx);
            }))
            .on_modifiers_changed(cx.listener(|this, event: &ModifiersChangedEvent, _, cx| {
                this.capture_hotkey_modifiers(event, cx);
            }))
            .into_any_element()
    }

    fn render_dictation_hotkey_alternatives(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = self.settings.dictation_hotkey_alternatives.len();
        let rows = (0..count)
            .map(|index| {
                let control =
                    self.render_hotkey_control(HotkeyKind::DictationAlternative(index), window, cx);
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(control)
                    .child(
                        compact_button("×")
                            .id(("remove-dictation-hotkey-alternative", index))
                            .size(px(28.0))
                            .px_0()
                            .justify_center()
                            .flex_none()
                            .text_size(px(14.0))
                            .text_color(rgb(MUTED))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.remove_dictation_hotkey_alternative(index, cx);
                            })),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        div()
            .flex()
            .flex_col()
            .items_end()
            .gap_2()
            .children(rows)
            .child(
                compact_plus_button()
                    .id("add-dictation-hotkey-alternative")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.add_dictation_hotkey_alternative(window, cx);
                    })),
            )
            .into_any_element()
    }

    fn render_hotkey_setting_control(
        &mut self,
        kind: HotkeyKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hotkey = self.render_hotkey_control(kind, window, cx);
        let side = self.hotkey_binding(kind).and_then(standalone_modifier_side);
        let side_animation = &mut self.hotkey_side_animations[hotkey_kind_index(kind)];
        side_animation.set_enabled(side.is_some());
        let side_position = side_animation.render_position(window).clamp(0.0, 1.0);
        let selected = side.unwrap_or(ModifierSide::Either);
        let side_widths = [34.0, 44.0, 34.0];
        let side_selection_spring =
            &mut self.hotkey_side_selection_springs[hotkey_kind_index(kind)];
        side_selection_spring.set_target(hotkey_side_index(selected) as f32);
        let selection_position = side_selection_spring.render_position(window);
        let (selection_left, selection_width) = segmented_geometry(selection_position, side_widths);
        let side_selector = div()
            .w(px(HOTKEY_SIDE_SELECTOR_WIDTH * side_position))
            .mr(px(8.0 * side_position))
            .flex_none()
            .overflow_hidden()
            .opacity(side_position)
            .child(
                segmented_control()
                    .w(px(HOTKEY_SIDE_SELECTOR_WIDTH))
                    .relative()
                    .child(
                        div()
                            .absolute()
                            .left(px(selection_left))
                            .top(px(2.0))
                            .w(px(selection_width))
                            .h(px(26.0))
                            .rounded(px(4.0))
                            .bg(rgb(SURFACE_SELECTED)),
                    )
                    .children(
                        [
                            ("Left", ModifierSide::Left),
                            ("Either", ModifierSide::Either),
                            ("Right", ModifierSide::Right),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, (label, side))| {
                            let candidate = hotkey_side_binding(&self.settings, kind, side);
                            segmented_item(selected == side)
                                .id(("hotkey-side", hotkey_kind_index(kind) * 3 + index))
                                .w(px(side_widths[index]))
                                .h(px(26.0))
                                .px(px(0.0))
                                .justify_center()
                                .bg(rgba(0x00000000))
                                .text_size(px(9.0))
                                .when(candidate.is_none(), |item| item.opacity(0.35))
                                .child(label)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let Some(candidate) =
                                        hotkey_side_binding(&this.settings, kind, side)
                                    else {
                                        return;
                                    };
                                    let Some(binding) = this.hotkey_binding_mut(kind) else {
                                        return;
                                    };
                                    *binding = candidate;
                                    this.hotkey_side_selection_springs[hotkey_kind_index(kind)]
                                        .set_target(index as f32);
                                    this.save_settings(cx);
                                }))
                        }),
                    ),
            );
        div()
            .flex()
            .items_center()
            .child(side_selector)
            .child(hotkey)
            .into_any_element()
    }

    fn hotkey_binding(&self, kind: HotkeyKind) -> Option<&HotkeyBinding> {
        match kind {
            HotkeyKind::Dictation => Some(&self.settings.dictation_hotkey),
            HotkeyKind::DictationAlternative(index) => {
                self.settings.dictation_hotkey_alternatives.get(index)
            }
            HotkeyKind::PasteLast => self.settings.paste_last_hotkey.as_ref(),
        }
    }

    fn hotkey_binding_mut(&mut self, kind: HotkeyKind) -> Option<&mut HotkeyBinding> {
        match kind {
            HotkeyKind::Dictation => Some(&mut self.settings.dictation_hotkey),
            HotkeyKind::DictationAlternative(index) => {
                self.settings.dictation_hotkey_alternatives.get_mut(index)
            }
            HotkeyKind::PasteLast => self.settings.paste_last_hotkey.as_mut(),
        }
    }

    fn add_dictation_hotkey_alternative(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let empty = HotkeyBinding {
            modifiers: HotkeyModifiers::default(),
            key: None,
        };
        self.settings.dictation_hotkey_alternatives.push(empty);
        let index = self.settings.dictation_hotkey_alternatives.len() - 1;
        self.begin_hotkey_capture(HotkeyKind::DictationAlternative(index), window, cx);
    }

    fn remove_dictation_hotkey_alternative(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.settings.dictation_hotkey_alternatives.len() {
            return;
        }
        self.cancel_hotkey_capture(cx);
        self.settings.dictation_hotkey_alternatives.remove(index);
        self.save_settings(cx);
        cx.notify();
    }

    fn begin_hotkey_capture(
        &mut self,
        kind: HotkeyKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::app_settings::set_hotkey_capture_active(true);
        self.hotkey_width_spring.set_target(hotkey_capture_width(0));
        self.hotkey_capture = HotkeyCaptureState::Listening {
            kind,
            modifiers: HotkeyModifiers::default(),
            message: None,
            started_at: Instant::now(),
        };
        self.hotkey_capture_animation.set_enabled(true);
        self.hotkey_focus.focus(window);
        cx.notify();
    }

    fn cancel_hotkey_capture(&mut self, cx: &mut Context<Self>) {
        if let HotkeyCaptureState::Listening {
            kind: HotkeyKind::DictationAlternative(index),
            ..
        } = self.hotkey_capture
            && self
                .settings
                .dictation_hotkey_alternatives
                .get(index)
                .is_some_and(HotkeyBinding::is_empty)
        {
            self.settings.dictation_hotkey_alternatives.remove(index);
        }
        if !matches!(self.hotkey_capture, HotkeyCaptureState::Idle) {
            crate::app_settings::set_hotkey_capture_active(false);
            self.hotkey_capture = HotkeyCaptureState::Idle;
            self.hotkey_capture_animation.set_enabled(false);
            self.hotkey_width_spring.set_target(HOTKEY_MIN_WIDTH);
            cx.notify();
        }
    }

    fn capture_hotkey_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if !self.hotkey_capture.is_listening() {
            return;
        }
        cx.stop_propagation();
        if event.is_held {
            return;
        }
        if event.keystroke.key == "escape" {
            self.cancel_hotkey_capture(cx);
            return;
        }
        let modifiers = hotkey_modifiers(event.keystroke.modifiers).without_side_constraints();
        let key = match hotkey_key(&event.keystroke.key) {
            Ok(key) => key,
            Err(message) => {
                self.set_hotkey_capture_message(message, cx);
                return;
            }
        };
        if modifiers.is_empty() && !is_function_key(&key.label) {
            self.set_hotkey_capture_message("Add a modifier", cx);
            return;
        }
        let binding = HotkeyBinding {
            modifiers,
            key: Some(key),
        };
        self.hotkey_width_spring
            .set_target(hotkey_capture_width(binding.keycaps().len()));
        self.save_hotkey_binding(binding, cx);
    }

    fn capture_hotkey_modifiers(&mut self, event: &ModifiersChangedEvent, cx: &mut Context<Self>) {
        let current = hotkey_modifiers(event.modifiers);
        if !current.is_empty() {
            self.hotkey_width_spring.set_target(hotkey_capture_width(
                HotkeyBinding {
                    modifiers: current,
                    key: None,
                }
                .keycaps()
                .len(),
            ));
        }
        let released = {
            let HotkeyCaptureState::Listening {
                modifiers, message, ..
            } = &mut self.hotkey_capture
            else {
                return;
            };
            if !current.is_empty() {
                if current.count() >= modifiers.count() {
                    *modifiers = current;
                    *message = None;
                }
                cx.notify();
                None
            } else if message.is_some() {
                *modifiers = HotkeyModifiers::default();
                *message = None;
                cx.notify();
                None
            } else {
                (!modifiers.is_empty()).then_some(*modifiers)
            }
        };
        if let Some(modifiers) = released {
            let modifiers = if modifiers.count() == 1 {
                modifiers
            } else {
                modifiers.without_side_constraints()
            };
            self.save_hotkey_binding(
                HotkeyBinding {
                    modifiers,
                    key: None,
                },
                cx,
            );
        }
    }

    fn set_hotkey_capture_message(&mut self, next_message: &'static str, cx: &mut Context<Self>) {
        if let HotkeyCaptureState::Listening { message, .. } = &mut self.hotkey_capture {
            *message = Some(next_message);
            cx.notify();
        }
    }

    fn save_hotkey_binding(&mut self, binding: HotkeyBinding, cx: &mut Context<Self>) {
        if binding.is_empty() {
            self.set_hotkey_capture_message("Press a shortcut", cx);
            return;
        }
        let kind = match self.hotkey_capture {
            HotkeyCaptureState::Listening { kind, .. } => kind,
            HotkeyCaptureState::Idle | HotkeyCaptureState::Saved { .. } => return,
        };
        if hotkey_binding_conflicts(&self.settings, kind, &binding) {
            self.set_hotkey_capture_message("Already in use", cx);
            return;
        }
        let key_based = binding.key.is_some();
        self.hotkey_width_spring
            .set_target(hotkey_saved_width(binding.keycaps().len()));
        match kind {
            HotkeyKind::Dictation => self.settings.dictation_hotkey = binding,
            HotkeyKind::DictationAlternative(index) => {
                if let Some(slot) = self.settings.dictation_hotkey_alternatives.get_mut(index) {
                    *slot = binding;
                }
            }
            HotkeyKind::PasteLast => self.settings.paste_last_hotkey = Some(binding),
        }
        if kind == HotkeyKind::Dictation && !key_based {
            self.settings.double_tap_only = false;
        }
        self.save_settings(cx);
        crate::app_settings::set_hotkey_capture_active(false);
        self.hotkey_capture = HotkeyCaptureState::Saved {
            kind,
            saved_at: Instant::now(),
        };
        cx.notify();
    }

    fn render_mode_replacements(
        &mut self,
        selection: ModeSelection,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let replacement_count = self.mode_inputs_for(selection).replacements.len();
        let replacement_rows = self
            .mode_inputs_for(selection)
            .replacements
            .iter()
            .enumerate()
            .map(|(index, inputs)| {
                div()
                    .id(("text-replacement", index))
                    .w_full()
                    .px_3()
                    .py_2()
                    .flex()
                    .items_end()
                    .gap_2()
                    .when(index + 1 < replacement_count, |row| {
                        row.border_b_1().border_color(rgb(LINE))
                    })
                    .child(compact_mode_field(
                        "Transcription",
                        inputs.matched_phrase.entity.clone(),
                    ))
                    .child(
                        div()
                            .h(px(34.0))
                            .flex()
                            .items_center()
                            .text_size(px(11.0))
                            .text_color(rgb(FAINT))
                            .child("→"),
                    )
                    .child(compact_mode_field("Output", inputs.output.entity.clone()))
                    .child(
                        compact_button("×")
                            .id(("remove-text-replacement", index))
                            .size(px(34.0))
                            .px_0()
                            .justify_center()
                            .flex_none()
                            .text_size(px(14.0))
                            .text_color(rgb(MUTED))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.mode_inputs_mut(selection).replacements.remove(index);
                                this.mode_settings_mut(selection).replacements.remove(index);
                                this.save_settings(cx);
                            })),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        let add = compact_header_plus_button()
            .id("add-mode-replacement")
            .on_click(cx.listener(move |this, _, window, cx| {
                let replacement = TextReplacement::default();
                let inputs = Self::replacement_inputs(&replacement, cx);
                let focus = inputs.matched_phrase.entity.focus_handle(cx);
                this.mode_inputs_mut(selection).replacements.push(inputs);
                this.mode_settings_mut(selection)
                    .replacements
                    .push(replacement);
                this.save_settings(cx);
                focus.focus(window);
            }))
            .into_any_element();

        compact_panel()
            .child(compact_panel_header("Corrections", Some(add)))
            .when(replacement_count == 0, |panel| {
                panel.child(
                    div()
                        .px_3()
                        .py_3()
                        .text_size(px(11.0))
                        .text_color(rgb(MUTED))
                        .child("No corrections in this mode."),
                )
            })
            .children(replacement_rows)
            .into_any_element()
    }

    fn transcription_picker_view(&mut self, selected_language: &str) -> TranscriptionPickerView {
        let models = crate::transcription_models::choices_for_runtime(selected_language)
            .into_iter()
            .map(|choice| {
                let model = choice.model;
                let installed = self.transcription_model_installed(model, selected_language);
                let downloading = self.transcription_downloading == Some(model.id);
                let active = transcription_selection_is_active(
                    &self.settings.transcription,
                    model,
                    selected_language,
                    installed,
                );
                let progress = if downloading {
                    model.download_bytes().map_or(0.0, |bytes| {
                        (self.transcription_downloaded_bytes as f32 / bytes as f32).clamp(0.0, 1.0)
                    })
                } else {
                    0.0
                };
                let status = if downloading {
                    if installed {
                        let elapsed = self
                            .transcription_activation_started
                            .map_or(0, |started| started.elapsed().as_secs());
                        let progress = self
                            .transcription_activation_started
                            .map_or(0.0, |started| (started.elapsed().as_secs_f32() % 1.6) / 1.6);
                        TranscriptionPickerStatus::Preparing {
                            label: format!("Loading model · {elapsed}s"),
                            progress: Some(TranscriptionPickerProgress::Loading(progress)),
                        }
                    } else {
                        TranscriptionPickerStatus::Preparing {
                            label: format!("Downloading {:.0}%", progress * 100.0),
                            progress: matches!(model.runtime, ModelRuntime::Gguf(_))
                                .then_some(TranscriptionPickerProgress::Downloading(progress)),
                        }
                    }
                } else if active {
                    TranscriptionPickerStatus::Active
                } else {
                    TranscriptionPickerStatus::Available { installed }
                };
                TranscriptionPickerModel { choice, status }
            })
            .collect();
        TranscriptionPickerView {
            error: self.transcription_picker_error.clone(),
            language: selected_language.to_string(),
            models,
        }
    }

    fn render_transcription_picker(
        &mut self,
        selected_language: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        render_shared_transcription_picker(self.transcription_picker_view(selected_language), cx)
    }

    fn dismiss_transcription_picker(&mut self, cx: &mut Context<Self>) {
        self.transcription_picker_language = None;
        self.clear_transcription_error();
        cx.notify();
    }

    fn sound_setting(&self, slot: SoundSlot) -> &str {
        match slot {
            SoundSlot::Start => &self.settings.start_sound,
            SoundSlot::Stop => &self.settings.stop_sound,
        }
    }

    fn render_sound_row(&self, slot: SoundSlot, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let label = crate::feedback::sound_label(self.sound_setting(slot));
        settings_row(
            slot.title(),
            slot.description(),
            disclosure_button(label)
                .id(slot.element_id("setting"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.sound_picker_open = if this.sound_picker_open == Some(slot) {
                        None
                    } else {
                        Some(slot)
                    };
                    cx.notify();
                })),
        )
        .id(slot.element_id("row"))
    }

    fn render_sound_picker(&self, slot: SoundSlot, cx: &mut Context<Self>) -> AnyElement {
        let current = self.sound_setting(slot).to_owned();
        let choices = std::iter::once(crate::feedback::DEFAULT_SOUND)
            .chain(
                crate::feedback::BUNDLED_SOUNDS
                    .iter()
                    .map(|(name, _)| *name),
            )
            .chain(crate::feedback::SYSTEM_SOUNDS.iter().copied())
            .collect::<Vec<_>>();
        div()
            .id(slot.element_id("picker"))
            .w_full()
            .px_4()
            .pb_3()
            .flex()
            .flex_wrap()
            .gap_1()
            .border_b_1()
            .border_color(rgb(LINE))
            .children(choices.into_iter().enumerate().map(|(index, choice)| {
                let selected = current == choice;
                div()
                    .id((slot.choice_prefix(), index))
                    .h(px(28.0))
                    .px_3()
                    .flex()
                    .items_center()
                    .rounded_sm()
                    .text_size(px(12.0))
                    .text_color(if selected { rgb(TEXT) } else { rgb(TEXT_SOFT) })
                    .when(selected, |row| row.bg(rgb(SURFACE_SELECTED)))
                    .hover(|row| row.bg(rgb(SURFACE_HOVER)))
                    .child(crate::feedback::sound_label(choice))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        match slot {
                            SoundSlot::Start => this.settings.start_sound = choice.into(),
                            SoundSlot::Stop => this.settings.stop_sound = choice.into(),
                        }
                        this.save_settings(cx);
                        crate::feedback::play(slot.tone());
                    }))
            }))
            .into_any_element()
    }

    fn render_microphone_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let choices = std::iter::once(None)
            .chain(self.microphone_devices.iter().cloned().map(Some))
            .collect::<Vec<_>>();
        div()
            .id("microphone-picker")
            .absolute()
            .top(px(170.0))
            .right(px(16.0))
            .w(px(220.0))
            .max_h(px(240.0))
            .p_2()
            .overflow_y_scroll()
            .rounded_sm()
            .border_1()
            .border_color(rgb(LINE))
            .bg(rgb(SURFACE))
            .shadow_lg()
            .children(choices.into_iter().enumerate().map(|(index, device)| {
                let selected = self.settings.microphone == device;
                let label = device.clone().unwrap_or_else(|| "Automatic".into());
                div()
                    .id(("microphone-choice", index))
                    .w_full()
                    .h(px(34.0))
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .rounded_sm()
                    .text_size(px(12.0))
                    .text_color(if selected { rgb(TEXT) } else { rgb(TEXT_SOFT) })
                    .when(selected, |row| row.bg(rgb(SURFACE_SELECTED)))
                    .hover(|row| row.bg(rgb(SURFACE_HOVER)))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings.microphone = device.clone();
                        this.microphone_picker_open = false;
                        this.microphone_picker_error = None;
                        this.save_settings(cx);
                    }))
            }))
            .when_some(self.microphone_picker_error.clone(), |picker, error| {
                picker.child(
                    div()
                        .px_3()
                        .pt_2()
                        .text_size(px(11.0))
                        .text_color(rgb(NEGATIVE))
                        .child(error),
                )
            })
            .into_any_element()
    }

    fn set_launch_at_login(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.preview {
            self.launch_at_login_status = if enabled {
                LoginItemStatus::Enabled
            } else {
                LoginItemStatus::Disabled
            };
            self.launch_at_login_toggle.set_enabled(enabled);
            cx.notify();
            return;
        }
        match crate::login_item::set_enabled(enabled) {
            Ok(status) => {
                self.launch_at_login_status = status;
                self.launch_at_login_error = None;
                self.launch_at_login_toggle
                    .set_enabled(status == LoginItemStatus::Enabled);
            }
            Err(error) => {
                self.launch_at_login_error = Some(error.to_string());
                if let Ok(status) = crate::login_item::status() {
                    self.launch_at_login_status = status;
                    self.launch_at_login_toggle
                        .set_enabled(status == LoginItemStatus::Enabled);
                }
            }
        }
        cx.notify();
    }

    fn render_permission_warnings(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let warnings = crate::onboarding::permission_warnings(self.setup_status);
        if self.setup_visible || warnings.is_empty() {
            return None;
        }
        Some(
            div()
                .child(settings_section_label("PERMISSIONS NEEDED"))
                .child(
                    settings_panel().children(warnings.into_iter().map(|warning| {
                        let (name, description) = permission_warning_copy(warning.kind);
                        let action = compact_button(permission_action_label(warning.action))
                            .id(permission_warning_id(warning.kind))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.perform_permission_action(warning);
                                cx.notify();
                            }));
                        settings_row(name, description, action)
                    })),
                )
                .into_any_element(),
        )
    }

    fn render_model_notice(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.setup_visible {
            return None;
        }
        let dictation_notice =
            dictation_model_notice(self.setup_status, self.transcription_downloading.is_some());
        let (title, description) = dictation_notice?;
        let action = compact_button("Choose model")
            .id("choose-required-model")
            .bg(rgb(ACCENT))
            .text_color(rgb(TEXT))
            .on_click(cx.listener(|this, _, _, cx| {
                this.transcription_picker_language =
                    Some(this.settings.transcription.language.clone());
                cx.notify();
            }))
            .into_any_element();
        Some(
            div()
                .id("dictation-model-notice")
                .w_full()
                .flex()
                .justify_center()
                .flex_shrink_0()
                .px_8()
                .py_4()
                .bg(rgb(SURFACE))
                .border_b_1()
                .border_color(rgb(LINE))
                .child(
                    div()
                        .w_full()
                        .max_w(px(PANE_CONTENT_WIDTH))
                        .flex()
                        .items_center()
                        .gap_4()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .border_l_2()
                                .border_color(rgb(ACCENT))
                                .pl_3()
                                .child(settings_copy(title, description)),
                        )
                        .child(div().flex_shrink_0().child(action)),
                )
                .into_any_element(),
        )
    }

    fn perform_permission_action(&mut self, warning: PermissionWarning) {
        match warning.action {
            PermissionAction::RequestMicrophone => crate::onboarding::request_microphone(),
            PermissionAction::OpenMicrophoneSettings => {
                crate::onboarding::open_permission_settings("microphone");
            }
            PermissionAction::OpenInputMonitoringSettings => {
                crate::permission_guide::show_input_monitoring();
            }
            PermissionAction::OpenAccessibilitySettings => {
                crate::permission_guide::show_accessibility();
            }
        }
        self.permission_refresh_at = Instant::now() + PERMISSION_REFRESH_INTERVAL;
    }

    fn render_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let permission_warnings = self.render_permission_warnings(cx);
        let hotkey_control = self.render_hotkey_setting_control(HotkeyKind::Dictation, window, cx);
        let alternative_controls = self.render_dictation_hotkey_alternatives(window, cx);
        let paste_last_control = self.render_hotkey_control(HotkeyKind::PasteLast, window, cx);
        let transcription_model = definition(self.settings.transcription.model);
        let transcription_label = transcription_model.name.to_owned();
        let transcription_control = disclosure_button(transcription_label)
            .id("transcription-model-setting")
            .on_click(cx.listener(|this, _, _, cx| {
                this.transcription_picker_language =
                    Some(this.settings.transcription.language.clone());
                this.clear_transcription_error();
                cx.notify();
            }));
        let double_tap_position = self.double_tap_toggle.render_position(window);
        let release_microphone_position = self.release_microphone_toggle.render_position(window);
        self.double_tap_only_visibility.set_enabled(
            self.settings.double_tap_lock && self.settings.dictation_hotkey.key.is_some(),
        );
        let double_tap_only_visibility = self
            .double_tap_only_visibility
            .render_position(window)
            .clamp(0.0, 1.0);
        let dock_icon_position = self.dock_icon_toggle.render_position(window);
        let launch_at_login_position = self.launch_at_login_toggle.render_position(window);
        let microphone_label = self
            .settings
            .microphone
            .clone()
            .unwrap_or_else(|| "Automatic".into());
        let microphone_picker = self
            .microphone_picker_open
            .then(|| self.render_microphone_picker(cx));
        let start_sound_row = self.render_sound_row(SoundSlot::Start, cx);
        let start_sound_picker = (self.sound_picker_open == Some(SoundSlot::Start))
            .then(|| self.render_sound_picker(SoundSlot::Start, cx));
        let stop_sound_row = self.render_sound_row(SoundSlot::Stop, cx);
        let stop_sound_picker = (self.sound_picker_open == Some(SoundSlot::Stop))
            .then(|| self.render_sound_picker(SoundSlot::Stop, cx));
        let sound_volume_position = self.sound_volume_spring.render_position(window);
        let sound_volume = segmented_control()
            .relative()
            .child(
                div()
                    .absolute()
                    .left(px(2.0 + sound_volume_position * 34.0))
                    .top(px(2.0))
                    .w(px(34.0))
                    .h(px(26.0))
                    .rounded(px(4.0))
                    .bg(rgb(SURFACE_SELECTED)),
            )
            .children(
                [
                    ("Off", 0.0_f32),
                    ("25%", 0.25),
                    ("50%", 0.5),
                    ("75%", 0.75),
                    ("100%", 1.0),
                ]
                .into_iter()
                .enumerate()
                .map(|(index, (label, volume))| {
                    let selected = if volume == 0.0 {
                        !self.settings.sound_effects
                    } else {
                        self.settings.sound_effects
                            && (self.settings.sound_effect_volume - volume).abs() < 0.01
                    };
                    segmented_item(selected)
                        .id(("sound-volume", index))
                        .w(px(34.0))
                        .px(px(0.0))
                        .justify_center()
                        .text_size(px(9.0))
                        .bg(rgba(0x00000000))
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.settings.sound_effects = volume > 0.0;
                            if volume > 0.0 {
                                this.settings.sound_effect_volume = volume;
                            }
                            this.sound_volume_spring.set_target(index as f32);
                            this.save_settings(cx);
                            if volume > 0.0 {
                                crate::feedback::play(crate::feedback::Tone::DictationStart);
                            }
                        }))
                }),
            );
        let launch_at_login_control =
            if self.launch_at_login_status == LoginItemStatus::RequiresApproval {
                compact_button("Open Settings")
                    .id("launch-at-login-approval")
                    .on_click(|_, _, _| crate::login_item::open_settings())
                    .into_any_element()
            } else {
                toggle(launch_at_login_position)
            };
        let recording_audio_position = self.recording_audio_spring.render_position(window);
        let audio_widths = [50.0, 90.0, 80.0];
        let (audio_left, audio_width) = segmented_geometry(recording_audio_position, audio_widths);
        let audio_behavior = segmented_control()
            .relative()
            .child(
                div()
                    .absolute()
                    .left(px(audio_left))
                    .top(px(2.0))
                    .w(px(audio_width))
                    .h(px(26.0))
                    .rounded(px(4.0))
                    .bg(rgb(SURFACE_SELECTED)),
            )
            .children(
                [
                    RecordingAudioBehavior::ALL[1],
                    RecordingAudioBehavior::ALL[2],
                    RecordingAudioBehavior::ALL[0],
                ]
                .into_iter()
                .enumerate()
                .map(|(index, behavior)| {
                    let selected = self.settings.recording_audio_behavior == behavior;
                    segmented_item(selected)
                        .id(("recording-audio-behavior", index))
                        .w(px(audio_widths[index]))
                        .px(px(0.0))
                        .justify_center()
                        .bg(rgba(0x00000000))
                        .child(behavior.label())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.settings.recording_audio_behavior == behavior {
                                return;
                            }
                            this.settings.recording_audio_behavior = behavior;
                            this.recording_audio_spring.set_target(index as f32);
                            this.save_settings(cx);
                        }))
                }),
            );
        let microphone_mode = segmented_control()
            .id("microphone-mode")
            .relative()
            .child(
                div()
                    .absolute()
                    .left(px(2.0 + release_microphone_position * 114.0))
                    .top(px(2.0))
                    .w(px(114.0))
                    .h(px(26.0))
                    .rounded(px(4.0))
                    .bg(rgb(SURFACE_SELECTED)),
            )
            .children(
                [("Keep ready (fast)", false), ("Release when idle", true)]
                    .into_iter()
                    .enumerate()
                    .map(|(index, (label, release))| {
                        segmented_item(self.settings.release_microphone_while_idle == release)
                            .id(("microphone-mode", index))
                            .w(px(114.0))
                            .px(px(0.0))
                            .justify_center()
                            .bg(rgba(0x00000000))
                            .child(label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.settings.release_microphone_while_idle != release
                                    && let Err(error) =
                                        this.set_release_microphone_while_idle(release, cx)
                                {
                                    tracing::error!(%error, "could not update microphone policy");
                                }
                                cx.notify();
                            }))
                    }),
            );
        settings_pane(
            div()
                                    .children(permission_warnings)
                                    .child(settings_section_label("DICTATION"))
                                    .child(
                                        settings_panel()
                                    .child(settings_row(
                                        "Local transcription",
                                        "Language and on-device speech model",
                                        transcription_control,
                                    ))
                                    .child(settings_row(
                                        "Microphone",
                                        "Automatically chooses the preferred available input",
                                        disclosure_button(microphone_label)
                                            .id("microphone-setting")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.microphone_picker_open =
                                                    !this.microphone_picker_open;
                                                if this.microphone_picker_open {
                                                    match crate::audio::input_device_names() {
                                                        Ok(devices) => {
                                                            this.microphone_devices = devices;
                                                            this.microphone_picker_error = None;
                                                        }
                                                        Err(error) => {
                                                            this.microphone_picker_error =
                                                                Some(error.to_string());
                                                        }
                                                    }
                                                }
                                                cx.notify();
                                            })),
                                    ))
                                    .when(
                                        transcription_model.supports_recognition_hints,
                                        |settings| {
                                            settings.child(settings_row(
                                                "Recognition hints",
                                                "Names and terms to softly prime the speech model",
                                                div()
                                                    .w(px(320.0))
                                                    .h(px(76.0))
                                                    .child(
                                                        self.transcription_hints.entity.clone(),
                                                    ),
                                            ))
                                        },
                                    )
                                    .child(
                                        settings_row(
                                            "Dictation shortcut",
                                            "Hold to dictate, release to transcribe",
                                            hotkey_control,
                                        )
                                        .id("dictation-hotkey-setting"),
                                    )
                                    .child(
                                        settings_row(
                                            "Alternative shortcuts",
                                            "Additional keys that start the same dictation",
                                            alternative_controls,
                                        )
                                        .border_b_0()
                                        .id("dictation-hotkey-alternatives-setting"),
                                    ),
                            )
                            .child(settings_section_label("BEHAVIOR"))
                            .child(
                                settings_panel()
                                    .child(
                                        settings_row(
                                            "While dictating",
                                            "Control other audio once a shortcut hold becomes intentional",
                                            audio_behavior,
                                        )
                                        .id("recording-audio-setting"),
                                    )
                                    .child(
                                        div()
                                            .w_full()
                                            .min_h(px(72.0))
                                            .px_4()
                                            .py_3()
                                            .flex()
                                            .items_center()
                                            .justify_between()
                                            .gap_4()
                                            .border_b_1()
                                            .border_color(rgb(LINE))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .child(settings_copy(
                                                        "Microphone mode",
                                                        if self.settings.release_microphone_while_idle {
                                                            "Opens on the shortcut, with a start-up delay and no pre-roll."
                                                        } else {
                                                            "Default: keeps the microphone open for the fastest start. A short pre-roll helps catch the beginning of speech. Audio is not saved by default."
                                                        },
                                                    )),
                                            )
                                            .child(microphone_mode),
                                    )
                                    .child(
                                        settings_row(
                                            "Double-tap to lock",
                                            "Double-tap the shortcut for hands-free dictation",
                                            toggle(double_tap_position),
                                        )
                                        .border_b_0()
                                        .id("double-tap-setting")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            let enabled = !this.snapshot().double_tap_lock;
                                            if let Err(error) = this.dispatch(
                                                DesktopAction::SetDoubleTapLock(enabled),
                                            ) {
                                                tracing::error!(%error, "could not update double-tap setting");
                                            }
                                            cx.notify();
                                        })),
                                    )
                                    .child(
                                        div()
                                            .h(px(72.0 * double_tap_only_visibility))
                                            .overflow_hidden()
                                            .opacity(double_tap_only_visibility)
                                            .child(
                                                settings_row(
                                                    "Double-tap only",
                                                    "Wait for two complete shortcut taps before recording",
                                                    toggle(if self.settings.double_tap_only { 1.0 } else { 0.0 }),
                                                )
                                                .id("double-tap-only-setting")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    let enabled = !this.snapshot().double_tap_only;
                                                    if let Err(error) = this.dispatch(
                                                        DesktopAction::SetDoubleTapOnly(enabled),
                                                    ) {
                                                        tracing::error!(%error, "could not update double-tap-only setting");
                                                    }
                                                    cx.notify();
                                                })),
                                            ),
                                    )
                                    .child(
                                        settings_row(
                                            "Paste last dictation",
                                            "Pastes the last ordinary dictation from this app session",
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_2()
                                                .child(paste_last_control)
                                                .child(
                                                    compact_button("Disable")
                                                        .id("disable-paste-last-hotkey")
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.settings.paste_last_hotkey = None;
                                                            this.save_settings(cx);
                                                        })),
                                                ),
                                        )
                                        .border_b_0()
                                        .id("paste-last-hotkey-setting"),
                                    )
                            )
                            .child(settings_section_label("APPLICATION"))
                            .child(
                                settings_panel()
                                    .child(
                                        settings_row(
                                            "Launch at login",
                                            "Start HEX automatically when you sign in to your Mac",
                                            launch_at_login_control,
                                        )
                                        .id("launch-at-login-setting")
                                        .when(
                                            self.launch_at_login_status
                                                != LoginItemStatus::RequiresApproval,
                                            |row| {
                                                row.on_click(cx.listener(
                                                    |this, _, _, cx| {
                                                        let enabled = this.launch_at_login_status
                                                            != LoginItemStatus::Enabled;
                                                        this.set_launch_at_login(enabled, cx);
                                                    },
                                                ))
                                            },
                                        ),
                                    )
                                    .when_some(
                                        self.launch_at_login_error.clone(),
                                        |settings, error| {
                                            settings.child(
                                                div()
                                                    .px_4()
                                                    .py_2()
                                                    .text_size(px(11.0))
                                                    .text_color(rgb(NEGATIVE))
                                                    .child(error),
                                            )
                                        },
                                    )
                                    .child(
                                        settings_row(
                                            "Show Dock icon",
                                            "When off, HEX starts quietly in the menu bar after setup",
                                            toggle(dock_icon_position),
                                        )
                                        .id("dock-icon-setting")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.settings.show_dock_icon =
                                                !this.settings.show_dock_icon;
                                            this.dock_icon_toggle
                                                .set_enabled(this.settings.show_dock_icon);
                                            crate::app_settings::set_dock_icon_visible(
                                                this.settings.show_dock_icon,
                                            );
                                            if this.settings.show_dock_icon {
                                                cx.activate(true);
                                            }
                                            this.save_settings(cx);
                                        })),
                                    )
                                    .child(
                                        settings_row(
                                            "Sound volume",
                                            "Recording, cancellation, and error tones",
                                            sound_volume,
                                        )
                                        .id("sound-effects-setting"),
                                    )
                                    .child(start_sound_row)
                                    .children(start_sound_picker)
                                    .child(stop_sound_row.border_b_0())
                                    .children(stop_sound_picker),
                            )
                             .children(microphone_picker),
        )
    }

    fn render_setup(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let status = self.setup_status;
        let microphone = match status.microphone {
            PermissionState::Ready => setup_ready_badge(),
            PermissionState::NeedsRequest => compact_button("Allow")
                .id("setup-microphone")
                .bg(rgb(SURFACE_SELECTED))
                .on_click(cx.listener(|_this, _, _, cx| {
                    crate::onboarding::request_microphone();
                    cx.notify();
                }))
                .into_any_element(),
            PermissionState::NeedsSettings => compact_button("Open Settings")
                .id("setup-microphone")
                .bg(rgb(SURFACE_SELECTED))
                .on_click(cx.listener(|_this, _, _, cx| {
                    crate::onboarding::open_permission_settings("microphone");
                    cx.notify();
                }))
                .into_any_element(),
        };
        let mut permission_rows = Vec::new();
        if status.microphone != PermissionState::Ready {
            permission_rows.push(setup_row(
                "Microphone",
                "Record dictation while you hold the shortcut.",
                microphone,
            ));
        }
        if status.input_monitoring != PermissionState::Ready {
            let input_monitoring = compact_button("Grant Access")
                .id("setup-input-monitoring")
                .bg(rgb(SURFACE_SELECTED))
                .on_click(cx.listener(|_this, _, _, cx| {
                    crate::permission_guide::show_input_monitoring();
                    cx.notify();
                }))
                .into_any_element();
            permission_rows.push(setup_row(
                "Input Monitoring",
                "Recognize the dictation shortcut anywhere.",
                input_monitoring,
            ));
        }
        if status.accessibility != PermissionState::Ready {
            let accessibility = compact_button("Grant Access")
                .id("setup-accessibility")
                .bg(rgb(SURFACE_SELECTED))
                .on_click(cx.listener(|_this, _, _, cx| {
                    crate::permission_guide::show_accessibility();
                    cx.notify();
                }))
                .into_any_element();
            permission_rows.push(setup_row(
                "Accessibility",
                "Paste completed dictation into the foreground app.",
                accessibility,
            ));
        }
        let models_ready = status.transcription_model;
        let model_notice = dictation_model_notice(status, self.transcription_downloading.is_some());
        let models = if models_ready {
            setup_ready_badge()
        } else {
            compact_button("Choose model")
                .id("setup-models")
                .bg(rgb(TEXT))
                .text_color(rgb(CANVAS))
                .font_weight(FontWeight::SEMIBOLD)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.transcription_picker_language =
                        Some(this.settings.transcription.language.clone());
                    this.clear_transcription_error();
                    cx.notify();
                }))
                .into_any_element()
        };

        div()
            .id("setup-backdrop")
            .occlude()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x000000dd))
            .child(
                div()
                    .id("setup")
                    .w(px(640.0))
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(LINE))
                    .bg(rgb(CANVAS))
                    .child(
                        div()
                            .px_7()
                            .pt_7()
                            .pb_5()
                            .child(
                                div()
                                    .text_size(px(22.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Set up HEX"),
                            )
                            .child(
                                div()
                                    .pt_2()
                                    .text_size(px(12.0))
                                    .line_height(px(19.0))
                                    .text_color(rgb(MUTED))
                                    .child("Everything stays on this Mac. HEX is ready once permissions and your local dictation model are set up."),
                            ),
                    )
                    .when(!permission_rows.is_empty(), |setup| {
                        setup.child(
                            div()
                                .mx_7()
                                .pb_5()
                                .child(setup_group_label("PERMISSIONS NEEDED"))
                                .child(div().border_t_1().border_color(rgb(LINE)).children(permission_rows)),
                        )
                    })
                    .child(
                        div()
                            .mx_7()
                            .child(setup_group_label("LOCAL MODELS"))
                            .child(
                                div().border_t_1().border_color(rgb(LINE)).child(setup_row(
                                    if let Some((title, _)) = model_notice {
                                        title
                                    } else {
                                        "Local dictation model"
                                    },
                                    if let Some((_, description)) = model_notice {
                                        description
                                    } else {
                                        "The selected model transcribes speech entirely on this Mac."
                                    },
                                    models,
                                )),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn save_settings(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = self.persist_settings() {
            tracing::error!(%error, "could not save app settings");
        }
        cx.notify();
    }

    fn persist_settings(&mut self) -> color_eyre::Result<()> {
        self.settings_save_generation = self.settings_save_generation.wrapping_add(1);
        if self.preview {
            self.settings_dirty = false;
            return Ok(());
        }
        self.settings.save()?;
        self.settings_dirty = false;
        self.settings_load_error = None;
        Ok(())
    }

    fn processing_input(
        placeholder: &'static str,
        initial: &str,
        cx: &mut Context<Self>,
    ) -> ProcessingInput {
        let entity = cx.new(|cx| TextInput::new(cx, placeholder, initial));
        Self::synchronized_processing_input(entity, cx)
    }

    fn replacement_inputs(
        replacement: &TextReplacement,
        cx: &mut Context<Self>,
    ) -> ReplacementInputs {
        ReplacementInputs {
            matched_phrase: Self::processing_input(
                "e.g. open code",
                &replacement.matched_phrase,
                cx,
            ),
            output: Self::processing_input("e.g. OpenCode", &replacement.output, cx),
        }
    }

    fn transcription_hints_input(initial: &str, cx: &mut Context<Self>) -> ProcessingInput {
        let entity = cx.new(|cx| {
            TextInput::multiline(
                cx,
                "Names and terms Whisper should expect, e.g. OpenCode, Effect...",
                initial,
            )
        });
        let subscription = cx.subscribe(&entity, |this, _, _: &TextChanged, cx| {
            this.settings.transcription.recognition_hints =
                this.transcription_hints.entity.read(cx).text().to_string();
            this.schedule_settings_save(cx);
        });
        ProcessingInput {
            entity,
            _subscriptions: vec![subscription],
        }
    }

    fn synchronized_processing_input(
        entity: Entity<TextInput>,
        cx: &mut Context<Self>,
    ) -> ProcessingInput {
        let subscription = cx.subscribe(&entity, |this, _, _: &TextChanged, cx| {
            this.sync_processing_settings(cx)
        });
        ProcessingInput {
            entity,
            _subscriptions: vec![subscription],
        }
    }

    fn application_search_input(cx: &mut Context<Self>) -> ProcessingInput {
        let entity = cx.new(|cx| TextInput::picker(cx, "Search applications", ""));
        let changed = cx.subscribe(&entity, |this, _, _: &TextChanged, cx| {
            this.application_picker_highlight = 0;
            cx.notify();
        });
        let navigate = cx.subscribe(&entity, |this, _, event: &TextNavigate, cx| {
            let count = this.application_choice_names(cx).len();
            if count > 0 {
                this.application_picker_highlight =
                    advance_highlight(this.application_picker_highlight, event.0, count);
                cx.notify();
            }
        });
        let submitted = cx.subscribe(&entity, |this, _, _: &TextSubmitted, cx| {
            if let Some(name) = this
                .application_choice_names(cx)
                .get(this.application_picker_highlight)
                .cloned()
            {
                this.add_mode_application(this.selected_mode, name, cx);
            }
        });
        let dismissed = cx.subscribe(&entity, |this, _, _: &TextDismissed, cx| {
            this.application_picker_open = false;
            this.application_picker_highlight = 0;
            this.application_search
                .entity
                .update(cx, |input, cx| input.set_text("", cx));
            cx.notify();
        });
        ProcessingInput {
            entity,
            _subscriptions: vec![changed, navigate, submitted, dismissed],
        }
    }

    fn mode_inputs(mode: &DictationMode, cx: &mut Context<Self>) -> ModeInputs {
        ModeInputs {
            name: Self::processing_input("Mode name", &mode.name, cx),
            browser_hosts: Self::processing_input(
                "github.com, x.com",
                &mode.browser_hosts.join(", "),
                cx,
            ),
            replacements: mode
                .replacements
                .iter()
                .map(|replacement| Self::replacement_inputs(replacement, cx))
                .collect(),
        }
    }

    fn processing_inputs(settings: &AppSettings, cx: &mut Context<Self>) -> ProcessingInputs {
        let processing = &settings.dictation_processing;
        ProcessingInputs {
            default_mode: Self::mode_inputs(&processing.default_mode, cx),
            modes: processing
                .modes
                .iter()
                .map(|mode| Self::mode_inputs(mode, cx))
                .collect(),
        }
    }

    fn sync_processing_settings(&mut self, cx: &mut Context<Self>) {
        apply_mode_inputs(
            &self.processing_inputs.default_mode,
            &mut self.settings.dictation_processing.default_mode,
            true,
            cx,
        );
        for (inputs, mode) in self
            .processing_inputs
            .modes
            .iter()
            .zip(&mut self.settings.dictation_processing.modes)
        {
            apply_mode_inputs(inputs, mode, false, cx);
        }
        self.schedule_settings_save(cx);
    }

    fn schedule_settings_save(&mut self, cx: &mut Context<Self>) {
        self.settings_dirty = true;
        self.settings_save_generation = self.settings_save_generation.wrapping_add(1);
        let generation = self.settings_save_generation;
        cx.spawn(async move |window, cx| {
            Timer::after(Duration::from_millis(250)).await;
            let _ = window.update(cx, |window, cx| {
                if window.settings_save_generation == generation {
                    window.save_settings(cx);
                }
            });
        })
        .detach();
    }

    fn render_modes(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let compact = window.viewport_size().width < px(980.0);
        let mode_list_width = if compact { 196.0 } else { 220.0 };
        let add = compact_header_plus_button()
            .id("add-dictation-mode")
            .border_1()
            .border_color(rgb(LINE))
            .bg(rgb(SURFACE))
            .on_click(cx.listener(|this, _, window, cx| {
                let mut mode = this.settings.dictation_processing.default_mode.clone();
                mode.name = format!("Mode {}", this.processing_inputs.modes.len() + 1);
                mode.applications.clear();
                mode.browser_hosts.clear();
                let mode = DictationMode { ..mode };
                this.processing_inputs
                    .modes
                    .push(Self::mode_inputs(&mode, cx));
                this.settings.dictation_processing.modes.push(mode);
                let selection =
                    ModeSelection::Custom(this.processing_inputs.modes.len().saturating_sub(1));
                this.select_mode(selection, window, cx);
                this.sync_processing_settings(cx);
            }));
        let default_selected = self.selected_mode == ModeSelection::Default;
        let mut rows = vec![
            mode_row(
                "Global",
                "Fallback for everything",
                default_selected,
                &[],
                &[],
                &self.application_catalog,
            )
            .id("default-dictation-mode")
            .on_click(cx.listener(|this, _, window, cx| {
                this.select_mode(ModeSelection::Default, window, cx);
            }))
            .into_any_element(),
        ];
        rows.extend(
            self.processing_inputs
                .modes
                .iter()
                .enumerate()
                .map(|(index, inputs)| {
                    let name = input_text(&inputs.name, cx);
                    let mode = &self.settings.dictation_processing.modes[index];
                    let selected = self.selected_mode == ModeSelection::Custom(index);
                    mode_row(
                        name,
                        "No activation rules",
                        selected,
                        &mode.applications,
                        &mode.browser_hosts,
                        &self.application_catalog,
                    )
                    .id(("dictation-mode", index))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.select_mode(ModeSelection::Custom(index), window, cx);
                            this.mode_context_menu = Some(event.position);
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_mode(ModeSelection::Custom(index), window, cx);
                    }))
                    .into_any_element()
                })
                .collect::<Vec<_>>(),
        );
        let detail = self.render_mode_detail(cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(pane_header_with_action(
                "Modes",
                Some(add.into_any_element()),
            ))
            .child(
                pane_body().p_5().child(
                    pane_content()
                        .flex_row()
                        .gap_5()
                        .child(
                            div()
                                .id("modes-list")
                                .w(px(mode_list_width))
                                .h_full()
                                .flex_none()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    compact_panel()
                                        .id("mode-list-card")
                                        .overflow_y_scroll()
                                        .child(
                                            div().p_2().flex().flex_col().gap_1().children(rows),
                                        ),
                                ),
                        )
                        .child(detail),
                ),
            )
            .into_any_element()
    }

    fn render_mode_context_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let position = self.mode_context_menu?;
        Some(
            div()
                .absolute()
                .left(position.x)
                .top(position.y)
                .w(px(148.0))
                .p_1()
                .rounded(px(8.0))
                .border_1()
                .border_color(rgb(0x444444))
                .bg(rgb(0x252525))
                .child(
                    div()
                        .id("context-delete-mode")
                        .h(px(30.0))
                        .px_2()
                        .flex()
                        .items_center()
                        .rounded(px(5.0))
                        .text_size(px(11.0))
                        .text_color(rgb(NEGATIVE))
                        .hover(|item| item.bg(rgb(SURFACE_HOVER)))
                        .child("Delete mode…")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.mode_delete_armed = true;
                                this.mode_context_menu = None;
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_mode_detail(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let selection = self.selected_mode;
        let is_default = selection == ModeSelection::Default;
        let (name, browser_hosts) = {
            let inputs = self.selected_mode_inputs_mut();
            (
                inputs.name.entity.clone(),
                inputs.browser_hosts.entity.clone(),
            )
        };
        let corrections = self.render_mode_replacements(selection, cx);
        let application_picker =
            (!is_default).then(|| self.render_application_picker(selection, cx));
        let basics = if is_default {
            compact_panel()
                .child(
                    div()
                        .min_h(px(64.0))
                        .px_3()
                        .py_2()
                        .flex()
                        .flex_col()
                        .justify_center()
                        .child(settings_copy(
                            "Global",
                            "Used unless a more specific mode matches.",
                        )),
                )
                .into_any_element()
        } else {
            compact_panel()
                .child(
                    div()
                        .min_h(px(54.0))
                        .px_3()
                        .py_2()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_4()
                        .border_b_1()
                        .border_color(rgb(LINE))
                        .child(
                            div()
                                .text_size(px(12.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(TEXT))
                                .child("Name"),
                        )
                        .child(div().w(px(300.0)).flex_none().child(name)),
                )
                .when_some(application_picker, |panel, picker| {
                    panel.child(div().border_b_1().border_color(rgb(LINE)).child(picker))
                })
                .child(
                    div()
                        .min_h(px(64.0))
                        .px_3()
                        .py_2()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_4()
                        .child(settings_copy(
                            "Web pages",
                            "Switch when a domain is active in Brave",
                        ))
                        .child(div().max_w(px(300.0)).flex_1().child(browser_hosts)),
                )
                .into_any_element()
        };

        let remove = (!is_default).then(|| {
            let action =
                if self.mode_delete_armed {
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(compact_button("Cancel").id("cancel-remove-mode").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.mode_delete_armed = false;
                                cx.notify();
                            }),
                        ))
                        .child(
                            compact_button("Confirm delete")
                                .id("confirm-remove-mode")
                                .border_1()
                                .border_color(rgb(0x613b3b))
                                .bg(rgb(0x271b1b))
                                .text_color(rgb(NEGATIVE))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let ModeSelection::Custom(index) = this.selected_mode else {
                                        return;
                                    };
                                    if index < this.processing_inputs.modes.len() {
                                        this.processing_inputs.modes.remove(index);
                                        this.settings.dictation_processing.modes.remove(index);
                                        this.selected_mode = ModeSelection::Default;
                                        this.mode_delete_armed = false;
                                        this.save_settings(cx);
                                    }
                                })),
                        )
                        .into_any_element()
                } else {
                    compact_button("Delete mode")
                        .id("remove-selected-mode")
                        .border_1()
                        .border_color(rgb(0x613b3b))
                        .bg(rgb(0x271b1b))
                        .text_color(rgb(NEGATIVE))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.mode_delete_armed = true;
                            cx.notify();
                        }))
                        .into_any_element()
                };
            div()
                .pt_3()
                .flex()
                .items_center()
                .justify_between()
                .border_t_1()
                .border_color(rgb(LINE))
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(rgb(FAINT))
                        .child("This cannot be undone."),
                )
                .child(action)
                .into_any_element()
        });

        div()
            .id("mode-detail")
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .overflow_y_scroll()
            .child(
                div()
                    .w_full()
                    .max_w(px(700.0))
                    .flex()
                    .flex_col()
                    .gap(px(SECTION_GAP))
                    .pb_5()
                    .child(basics)
                    .child(div().h(px(4.0)))
                    .child(compact_section_label("TEXT PROCESSING"))
                    .child(corrections)
                    .when_some(remove, |detail, remove| {
                        detail.child(div().pt_1().child(remove))
                    }),
            )
            .into_any_element()
    }

    fn render_application_picker(
        &self,
        selection: ModeSelection,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.mode_settings(selection).applications.clone();
        let selected_rows = selected.iter().enumerate().map(|(index, name)| {
            let application = self.application_named(name);
            let name = name.clone();
            div()
                .id(("selected-application", index))
                .h(px(30.0))
                .pl_1()
                .pr_1()
                .flex()
                .items_center()
                .gap_1()
                .rounded_sm()
                .bg(rgb(SURFACE_SELECTED))
                .border_1()
                .border_color(rgb(LINE))
                .child(application_icon(application, Some(name.as_str()), 20.0))
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(rgb(TEXT_SOFT))
                        .child(name.clone()),
                )
                .child(
                    div()
                        .id(("remove-selected-application", index))
                        .size(px(18.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_sm()
                        .text_size(px(14.0))
                        .text_color(rgb(FAINT))
                        .hover(|button| button.bg(rgb(SURFACE_HOVER)).text_color(rgb(TEXT_SOFT)))
                        .child("×")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.remove_mode_application(selection, &name, cx);
                        })),
                )
        });
        let query = self
            .application_search
            .entity
            .read(cx)
            .text()
            .trim()
            .to_lowercase();
        let available = if self.application_picker_open {
            self.application_choices(selection, cx)
        } else {
            Vec::new()
        };
        let no_matches = available.is_empty();
        let available_rows =
            available
                .into_iter()
                .cloned()
                .enumerate()
                .map(|(index, application)| {
                    let name = application.name.clone();
                    let location = application
                        .path
                        .parent()
                        .map_or_else(String::new, |path| path.display().to_string());
                    div()
                        .id(("available-application", index))
                        .w_full()
                        .px_2()
                        .py_2()
                        .flex()
                        .items_center()
                        .gap_3()
                        .border_b_1()
                        .border_color(rgb(LINE))
                        .when(index == self.application_picker_highlight, |row| {
                            row.bg(rgb(SURFACE_SELECTED))
                        })
                        .hover(|row| row.bg(rgb(SURFACE_HOVER)))
                        .child(application_icon(Some(&application), Some(&name), 30.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(rgb(TEXT_SOFT))
                                        .child(name.clone()),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(rgb(FAINT))
                                        .overflow_hidden()
                                        .child(location),
                                ),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.add_mode_application(selection, name.clone(), cx);
                            }),
                        )
                });
        let picker_status = match &self.application_catalog {
            ApplicationCatalogState::Loading => Some("Finding applications on this Mac...".into()),
            ApplicationCatalogState::Loaded(applications) if applications.is_empty() => {
                Some("No applications found.".into())
            }
            ApplicationCatalogState::Loaded(_) if no_matches && !query.is_empty() => {
                Some(format!("No applications match \"{query}\"."))
            }
            _ => None,
        };
        let tray = self.application_picker_open.then(|| {
            div()
                .mt_2()
                .p_2()
                .rounded_sm()
                .border_1()
                .border_color(rgb(LINE))
                .bg(rgb(SURFACE))
                .child(self.application_search.entity.clone())
                .when_some(picker_status, |tray, status| {
                    tray.child(
                        div()
                            .px_2()
                            .py_3()
                            .text_size(px(11.0))
                            .text_color(rgb(MUTED))
                            .child(status),
                    )
                })
                .child(
                    div()
                        .id("application-picker-results")
                        .max_h(px(300.0))
                        .overflow_y_scroll()
                        .children(available_rows),
                )
                .child(
                    div()
                        .pt_2()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .id("choose-application-bundle")
                                .h(px(28.0))
                                .px_2()
                                .flex()
                                .items_center()
                                .rounded_sm()
                                .text_size(px(11.0))
                                .text_color(rgb(TEXT_SOFT))
                                .hover(|button| button.bg(rgb(SURFACE_HOVER)))
                                .child("Choose from Finder...")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.choose_application(selection, cx);
                                })),
                        )
                        .child(
                            div()
                                .id("close-application-picker")
                                .h(px(28.0))
                                .px_2()
                                .flex()
                                .items_center()
                                .rounded_sm()
                                .text_size(px(11.0))
                                .text_color(rgb(MUTED))
                                .hover(|button| button.bg(rgb(SURFACE_HOVER)))
                                .child("Done")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.application_picker_open = false;
                                    this.application_picker_highlight = 0;
                                    this.application_search
                                        .entity
                                        .update(cx, |input, cx| input.set_text("", cx));
                                    window.blur();
                                    cx.notify();
                                })),
                        ),
                )
        });
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .min_h(px(64.0))
                    .px_3()
                    .py_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(settings_copy(
                        "Applications",
                        "Switch when an app is frontmost",
                    ))
                    .child(
                        div()
                            .max_w(px(300.0))
                            .min_h(px(CONTROL_HEIGHT))
                            .flex_1()
                            .p_1()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_end()
                            .gap_1()
                            .children(selected_rows)
                            .child(
                                compact_plus_button()
                                    .id("open-application-picker")
                                    .size(px(30.0))
                                    .border_1()
                                    .border_color(rgb(LINE))
                                    .bg(rgb(CANVAS))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.application_picker_highlight = 0;
                                        this.application_picker_open = true;
                                        this.application_picker_error = None;
                                        this.ensure_application_catalog_load();
                                        this.application_search
                                            .entity
                                            .update(cx, |input, cx| input.set_text("", cx));
                                        this.application_search
                                            .entity
                                            .focus_handle(cx)
                                            .focus(window);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .when_some(self.application_picker_error.clone(), |picker, error| {
                picker.child(
                    div()
                        .text_size(px(10.0))
                        .text_color(rgb(NEGATIVE))
                        .child(error),
                )
            })
            .when_some(tray, |picker, tray| picker.child(tray))
            .into_any_element()
    }

    fn application_named(&self, name: &str) -> Option<&InstalledApplication> {
        let ApplicationCatalogState::Loaded(applications) = &self.application_catalog else {
            return None;
        };
        applications
            .iter()
            .find(|application| application.name == name)
    }

    fn application_choice_names(&self, cx: &App) -> Vec<String> {
        self.application_choices(self.selected_mode, cx)
            .into_iter()
            .map(|application| application.name.clone())
            .collect()
    }

    fn application_choices<'a>(
        &'a self,
        selection: ModeSelection,
        cx: &App,
    ) -> Vec<&'a InstalledApplication> {
        let ApplicationCatalogState::Loaded(applications) = &self.application_catalog else {
            return Vec::new();
        };
        let selected = &self.mode_settings(selection).applications;
        let query = self
            .application_search
            .entity
            .read(cx)
            .text()
            .trim()
            .to_lowercase();
        applications
            .iter()
            .filter(|application| {
                !selected.contains(&application.name)
                    && (query.is_empty()
                        || application.name.to_lowercase().contains(&query)
                        || application
                            .bundle_id
                            .as_deref()
                            .is_some_and(|bundle| bundle.to_lowercase().contains(&query)))
            })
            .take(10)
            .collect()
    }

    fn add_mode_application(
        &mut self,
        selection: ModeSelection,
        name: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(conflict) = self
            .settings
            .dictation_processing
            .modes
            .iter()
            .enumerate()
            .find(|(index, mode)| {
                selection != ModeSelection::Custom(*index) && mode.applications.contains(&name)
            })
            .map(|(_, mode)| mode.name.clone())
        {
            self.application_picker_error = Some(format!(
                "{name} already activates {conflict}. Remove it there before reassigning it."
            ));
            cx.notify();
            return;
        }
        let applications = &mut self.mode_settings_mut(selection).applications;
        if !applications.contains(&name) {
            applications.push(name);
            applications.sort_by_key(|name| name.to_lowercase());
            self.application_picker_highlight = 0;
            self.application_search
                .entity
                .update(cx, |input, cx| input.set_text("", cx));
            self.application_picker_error = None;
            self.save_settings(cx);
        }
    }

    fn remove_mode_application(
        &mut self,
        selection: ModeSelection,
        name: &str,
        cx: &mut Context<Self>,
    ) {
        self.mode_settings_mut(selection)
            .applications
            .retain(|application| application != name);
        self.save_settings(cx);
    }

    fn choose_application(&mut self, selection: ModeSelection, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Add Application".into()),
        });
        cx.spawn(async move |window, cx| {
            let path = match receiver.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) => None,
                Ok(Err(error)) => {
                    let _ = window.update(cx, |window, cx| {
                        window.application_picker_error = Some(error.to_string());
                        cx.notify();
                    });
                    return;
                }
                Err(error) => {
                    let _ = window.update(cx, |window, cx| {
                        window.application_picker_error = Some(error.to_string());
                        cx.notify();
                    });
                    return;
                }
            };
            let Some(path) = path else {
                return;
            };
            let result = cx
                .background_executor()
                .spawn(async move { crate::application_catalog::metadata(&path) })
                .await;
            let _ = window.update(cx, |window, cx| match result {
                Ok(application) => {
                    let name = application.name.clone();
                    match &mut window.application_catalog {
                        ApplicationCatalogState::Loading => {
                            window.application_catalog =
                                ApplicationCatalogState::Loaded(vec![application]);
                        }
                        ApplicationCatalogState::Loaded(applications) => {
                            crate::application_catalog::insert(applications, application);
                        }
                    }
                    window.application_picker_error = None;
                    window.add_mode_application(selection, name, cx);
                }
                Err(error) => {
                    window.application_picker_error = Some(error.to_string());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn selected_mode_inputs_mut(&mut self) -> &mut ModeInputs {
        self.mode_inputs_mut(self.selected_mode)
    }

    fn select_mode(
        &mut self,
        selection: ModeSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_mode = selection;
        self.application_picker_open = false;
        self.application_picker_error = None;
        self.application_picker_highlight = 0;
        self.mode_delete_armed = false;
        self.mode_context_menu = None;
        self.application_search
            .entity
            .update(cx, |input, cx| input.set_text("", cx));
        window.blur();
        cx.notify();
    }

    fn mode_inputs_mut(&mut self, selection: ModeSelection) -> &mut ModeInputs {
        match selection {
            ModeSelection::Default => &mut self.processing_inputs.default_mode,
            ModeSelection::Custom(index) => &mut self.processing_inputs.modes[index],
        }
    }

    fn mode_inputs_for(&self, selection: ModeSelection) -> &ModeInputs {
        match selection {
            ModeSelection::Default => &self.processing_inputs.default_mode,
            ModeSelection::Custom(index) => &self.processing_inputs.modes[index],
        }
    }

    fn mode_settings(&self, selection: ModeSelection) -> &DictationMode {
        match selection {
            ModeSelection::Default => &self.settings.dictation_processing.default_mode,
            ModeSelection::Custom(index) => &self.settings.dictation_processing.modes[index],
        }
    }

    fn mode_settings_mut(&mut self, selection: ModeSelection) -> &mut DictationMode {
        match selection {
            ModeSelection::Default => &mut self.settings.dictation_processing.default_mode,
            ModeSelection::Custom(index) => &mut self.settings.dictation_processing.modes[index],
        }
    }

    fn render_activity(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let snapshot = self.snapshot();
        let refresh = header_button("Refresh")
            .id("refresh-activity")
            .on_click(cx.listener(|this, _, _, cx| {
                this.reload_events();
                cx.notify();
            }))
            .into_any_element();
        let header_action = div()
            .flex()
            .items_center()
            .gap_4()
            .when_some(snapshot.activity.state_label(), |actions, status| {
                let active = snapshot.activity.state != Some(VoiceState::Stopping);
                actions.child(listener_status(
                    status,
                    snapshot.activity.device.clone().unwrap_or_default(),
                    active,
                ))
            })
            .child(refresh)
            .into_any_element();
        let rows: Vec<AnyElement> = self
            .events
            .iter()
            .enumerate()
            .map(|(index, event)| {
                let selected = self.selected_event == Some(index);
                let summary = event_summary(event);
                let age = event_age(event.timestamp_ms());
                let boundary = matches!(event, VoiceEvent::SessionStarted { .. });
                div()
                    .id(("activity-event", index))
                    .w_full()
                    .px_4()
                    .py_3()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap_4()
                    .border_b_1()
                    .border_color(rgb(LINE))
                    .when(selected, |row| row.bg(rgb(SURFACE_SELECTED)))
                    .hover(|row| row.bg(rgb(SURFACE_HOVER)))
                    .child(
                        div()
                            .flex_1()
                            .text_size(if boundary { px(11.0) } else { px(12.0) })
                            .font_weight(if boundary {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(if boundary { rgb(FAINT) } else { rgb(TEXT_SOFT) })
                            .line_height(px(18.0))
                            .child(summary),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(rgb(FAINT))
                            .child(age),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected_event = Some(index);
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(pane_header_with_action("Activity", Some(header_action)))
            .child(
                pane_body().child(
                    pane_content()
                        .flex_row()
                        .child(
                            div()
                                .id("activity-list")
                                .w(px(PANE_LIST_WIDTH))
                                .h_full()
                                .flex_none()
                                .overflow_y_scroll()
                                .border_r_1()
                                .border_color(rgb(LINE))
                                .when(
                                    self.events.is_empty() && self.activity.error.is_none(),
                                    |list| list.child(empty_message("No activity yet.")),
                                )
                                .when_some(self.activity.error.clone(), |list, error| {
                                    list.child(error_message(
                                        "Activity could not be loaded.",
                                        error,
                                    ))
                                })
                                .children(rows),
                        )
                        .child(self.render_event_detail()),
                ),
            )
            .into_any_element()
    }

    fn render_event_detail(&self) -> AnyElement {
        let Some(event) = self.selected_event.and_then(|index| self.events.get(index)) else {
            return detail_placeholder("Select an activity row.");
        };
        let raw = serde_json::to_string(event).unwrap_or_else(|error| error.to_string());
        div()
            .id("activity-detail")
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .overflow_y_scroll()
            .px_6()
            .py_6()
            .child(
                div()
                    .text_size(px(18.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .line_height(px(24.0))
                    .child(event_summary(event)),
            )
            .child(
                div()
                    .pt_6()
                    .child(detail_row("Event", event.kind()))
                    .child(detail_row(
                        "Timestamp",
                        format!("{} ms", event.timestamp_ms()),
                    )),
            )
            .child(
                div()
                    .mt_6()
                    .pt_5()
                    .border_t_1()
                    .border_color(rgb(LINE))
                    .child(section_label("Raw event"))
                    .child(
                        div()
                            .pt_3()
                            .text_size(px(11.0))
                            .line_height(px(18.0))
                            .text_color(rgb(MUTED))
                            .child(raw),
                    ),
            )
            .into_any_element()
    }
    fn apply_hud_lab(&self) {
        self.indicator
            .send(DictationIndicatorEvent::Configure(self.hud_tuning));
        self.indicator.send(DictationIndicatorEvent::Reset);
        match self.hud_demo {
            HudDemoState::Recording => {
                for job_id in 0..self.hud_queued as u64 {
                    self.indicator
                        .send(DictationIndicatorEvent::Submitted { job_id });
                    self.indicator
                        .send(DictationIndicatorEvent::Transcribing { job_id });
                }
                self.indicator.send(DictationIndicatorEvent::Started);
                self.indicator.send(DictationIndicatorEvent::Meter {
                    average: 0.08,
                    peak: 0.42,
                });
            }
            HudDemoState::Transcribing | HudDemoState::Processing => {
                self.indicator
                    .send(DictationIndicatorEvent::Submitted { job_id: 0 });
                self.indicator.send(match self.hud_demo {
                    HudDemoState::Transcribing => {
                        DictationIndicatorEvent::Transcribing { job_id: 0 }
                    }
                    HudDemoState::Processing => DictationIndicatorEvent::Processing { job_id: 0 },
                    HudDemoState::Recording => unreachable!(),
                });
                for job_id in 1..=self.hud_queued as u64 {
                    self.indicator
                        .send(DictationIndicatorEvent::Submitted { job_id });
                    self.indicator
                        .send(DictationIndicatorEvent::Transcribing { job_id });
                }
            }
        }
    }

    fn hud_control_value(&self, control: HudControl) -> f32 {
        match control {
            HudControl::LineCount => (self.hud_tuning.line_count - 1.0) / 5.0,
            HudControl::Curvature => self.hud_tuning.curvature,
            HudControl::Speed => self.hud_tuning.speed / HUD_MAX_SPEED,
            HudControl::Blur => 1.0 - self.hud_tuning.sharpness,
            HudControl::Glow => self.hud_tuning.glow,
            HudControl::Depth => self.hud_tuning.depth,
            HudControl::LightAngle => self.hud_tuning.light_angle,
            HudControl::Outline => self.hud_tuning.outline,
        }
        .clamp(0.0, 1.0)
    }

    fn set_hud_control(&mut self, control: HudControl, value: f32, cx: &mut Context<Self>) {
        let value = value.clamp(0.0, 1.0);
        match control {
            HudControl::LineCount => self.hud_tuning.line_count = (1.0 + value * 5.0).round(),
            HudControl::Curvature => self.hud_tuning.curvature = value,
            HudControl::Speed => self.hud_tuning.speed = value * HUD_MAX_SPEED,
            HudControl::Blur => self.hud_tuning.sharpness = 1.0 - value,
            HudControl::Glow => self.hud_tuning.glow = value,
            HudControl::Depth => self.hud_tuning.depth = value,
            HudControl::LightAngle => self.hud_tuning.light_angle = value,
            HudControl::Outline => self.hud_tuning.outline = value,
        }
        self.hud_copied = false;
        self.indicator
            .send(DictationIndicatorEvent::Configure(self.hud_tuning));
        cx.notify();
    }

    fn update_hud_from_pointer(
        &mut self,
        control: HudControl,
        pointer_x: f32,
        cx: &mut Context<Self>,
    ) {
        const SLIDER_LEFT: f32 = SIDEBAR_WIDTH + 24.0;
        const SLIDER_WIDTH: f32 = 360.0;
        self.set_hud_control(control, (pointer_x - SLIDER_LEFT) / SLIDER_WIDTH, cx);
    }

    fn select_hud_style(&mut self, style: usize, cx: &mut Context<Self>) {
        self.hud_tuning = match style {
            0 => HudTuning {
                style: 0.0,
                line_count: 1.0,
                curvature: 0.5,
                speed: 0.7,
                sharpness: 0.42,
                glow: 0.4,
                depth: 0.75,
                light_angle: 0.35,
                outline: 0.25,
            },
            1 => HudTuning {
                style: 1.0,
                line_count: 2.0,
                curvature: 0.47,
                speed: 17.08,
                sharpness: 0.29,
                glow: 0.77,
                depth: 0.65,
                light_angle: 0.35,
                outline: 1.0,
            },
            _ => HudTuning {
                style: 2.0,
                line_count: 3.0,
                curvature: 0.7,
                speed: 0.9,
                sharpness: 0.64,
                glow: 0.28,
                depth: 0.74,
                light_angle: 0.35,
                outline: 0.3,
            },
        };
        self.hud_copied = false;
        self.apply_hud_lab();
        cx.notify();
    }

    fn hud_preset(&self) -> String {
        format!(
            "hud-v1 style={} lines={:.0} curvature={:.2} speed={:.2} blur={:.2} glow={:.2} depth={:.2} light={:.2} outline={:.2}",
            self.hud_tuning.style as usize,
            self.hud_tuning.line_count,
            self.hud_tuning.curvature,
            self.hud_tuning.speed,
            1.0 - self.hud_tuning.sharpness,
            self.hud_tuning.glow,
            self.hud_tuning.depth,
            self.hud_tuning.light_angle,
            self.hud_tuning.outline,
        )
    }

    fn render_hud_slider(
        &self,
        control: HudControl,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let value = self.hud_control_value(control);
        let display = match control {
            HudControl::LineCount => format!("{:.0}", self.hud_tuning.line_count),
            HudControl::Speed => format!("{:.2}x", self.hud_tuning.speed),
            HudControl::LightAngle => format!("{:.0}°", self.hud_tuning.light_angle * 360.0),
            _ => format!("{:.2}", value),
        };
        div()
            .w(px(360.0))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .text_size(px(12.0))
                    .child(label)
                    .child(div().text_color(rgb(MUTED)).child(display)),
            )
            .child(
                div()
                    .id(("hud-slider", control as usize))
                    .relative()
                    .w_full()
                    .h(px(18.0))
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .top(px(7.0))
                            .h(px(4.0))
                            .rounded_full()
                            .bg(rgb(0x303030)),
                    )
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .top(px(7.0))
                            .w(relative(value))
                            .h(px(4.0))
                            .rounded_full()
                            .bg(rgb(0x6f7fff)),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(relative(value))
                            .top(px(2.0))
                            .ml(px(-7.0))
                            .size(px(14.0))
                            .rounded_full()
                            .border_2()
                            .border_color(rgb(0xe8e8e8))
                            .bg(rgb(0x333333)),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            this.hud_dragging = Some(control);
                            this.update_hud_from_pointer(control, f32::from(event.position.x), cx);
                        }),
                    )
                    .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                        if event.dragging() && this.hud_dragging == Some(control) {
                            this.update_hud_from_pointer(control, f32::from(event.position.x), cx);
                        }
                    })),
            )
            .into_any_element()
    }

    fn render_hud_lab(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let styles = [
            (
                "A",
                "Moving light",
                "A line-free area light travels around the orb.",
            ),
            (
                "B",
                "Meridians",
                "Highlights wrap around the sphere instead of crossing it.",
            ),
            (
                "C",
                "Ribbons",
                "Curved bands retained for comparison and tuning.",
            ),
        ];
        let style_cards =
            styles
                .into_iter()
                .enumerate()
                .map(|(index, (key, name, description))| {
                    let selected = self.hud_tuning.style as usize == index;
                    div()
                        .id(("hud-style", index))
                        .w(px(190.0))
                        .p_3()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(if selected { 0x697cff } else { LINE }))
                        .bg(rgb(if selected { 0x1c2030 } else { SURFACE }))
                        .hover(|card| card.bg(rgb(SURFACE_HOVER)))
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(rgb(0x8390ff))
                                .child(key),
                        )
                        .child(
                            div()
                                .text_size(px(12.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(name),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(rgb(MUTED))
                                .child(description),
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.select_hud_style(index, cx)),
                        )
                });
        let state_button = |state, label: &'static str, index: usize| {
            compact_button(label)
                .id(("hud-state", index))
                .when(self.hud_demo == state, |button| {
                    button.bg(rgb(SURFACE_SELECTED)).text_color(rgb(TEXT))
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.hud_demo = state;
                    this.apply_hud_lab();
                    cx.notify();
                }))
        };
        let queue_buttons = (0..=3).map(|count| {
            compact_button(count.to_string())
                .id(("hud-queue", count))
                .when(self.hud_queued == count, |button| {
                    button.bg(rgb(SURFACE_SELECTED)).text_color(rgb(TEXT))
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.hud_queued = count;
                    this.apply_hud_lab();
                    cx.notify();
                }))
        });
        let preset = self.hud_preset();
        let copied = self.hud_copied;

        div()
            .size_full()
            .flex()
            .flex_col()
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.hud_dragging = None;
                }),
            )
            .child(pane_header_with_action(
                "HUD Lab",
                Some(
                    header_button(if copied { "Copied" } else { "Copy preset" })
                        .id("copy-hud-preset")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                let _ = clipboard.set_text(this.hud_preset());
                                this.hud_copied = true;
                                cx.notify();
                            }
                        }))
                        .into_any_element(),
                ),
            ))
            .child(
                pane_content()
                    .id("hud-lab-scroll")
                    .flex_1()
                    .mx_auto()
                    .overflow_y_scroll()
                    .p_6()
                    .gap_6()
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .children([
                                state_button(HudDemoState::Recording, "Recording", 0),
                                state_button(HudDemoState::Transcribing, "Transcribing", 1),
                                state_button(HudDemoState::Processing, "Processing", 2),
                            ])
                            .child(div().w_4())
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(rgb(MUTED))
                                    .child("Queued"),
                            )
                            .children(queue_buttons),
                    )
                    .child(div().flex().gap_3().children(style_cards))
                    .child(
                        div()
                            .flex()
                            .child(div().flex().flex_col().gap_4().children([
                                self.render_hud_slider(HudControl::LineCount, "Line count", cx),
                                self.render_hud_slider(HudControl::Curvature, "Curvature", cx),
                                self.render_hud_slider(HudControl::Speed, "Speed", cx),
                                self.render_hud_slider(HudControl::Blur, "Highlight blur", cx),
                                self.render_hud_slider(HudControl::Glow, "Line glow", cx),
                                self.render_hud_slider(HudControl::Depth, "Sphere depth", cx),
                                self.render_hud_slider(HudControl::LightAngle, "Light angle", cx),
                                self.render_hud_slider(HudControl::Outline, "Sphere outline", cx),
                            ])),
                    )
                    .child(
                        div()
                            .p_3()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(LINE))
                            .bg(rgb(0x0d0d0d))
                            .font_family("SF Mono")
                            .text_size(px(10.0))
                            .text_color(rgb(TEXT_SOFT))
                            .child(preset),
                    ),
            )
            .into_any_element()
    }
}

fn application_catalog_load_needed(
    preview: bool,
    catalog: &ApplicationCatalogState,
    receiver_pending: bool,
) -> bool {
    !preview && matches!(catalog, ApplicationCatalogState::Loading) && !receiver_pending
}

impl Drop for AppWindow {
    fn drop(&mut self) {
        if !self.preview
            && self.settings_dirty
            && let Err(error) = self.settings.save()
        {
            tracing::error!(%error, "could not flush app settings while closing window");
        }
        crate::app_settings::set_hotkey_capture_active(false);
    }
}

impl TranscriptionPickerDelegate for AppWindow {
    fn cancel_transcription_preparation(&mut self) {
        self.cancel_transcription_download();
    }

    fn choose_transcription_model(
        &mut self,
        model: TranscriptionModelId,
        language: String,
        cx: &mut Context<Self>,
    ) {
        AppWindow::choose_transcription_model(self, definition(model), language, cx);
    }

    fn dismiss_transcription_picker(&mut self, cx: &mut Context<Self>) {
        AppWindow::dismiss_transcription_picker(self, cx);
    }
}

impl DesktopHost for AppWindow {
    fn capabilities(&self) -> DesktopCapabilities {
        DesktopCapabilities::macos()
    }

    fn snapshot(&self) -> DesktopSnapshot {
        let update_status = match self.update_status {
            crate::sparkle::UpdateStatus::Unavailable => DesktopUpdateStatus::Unavailable,
            crate::sparkle::UpdateStatus::Checking => DesktopUpdateStatus::Checking,
            crate::sparkle::UpdateStatus::Idle | crate::sparkle::UpdateStatus::UpToDate => {
                DesktopUpdateStatus::Current
            }
            crate::sparkle::UpdateStatus::UpdateAvailable => DesktopUpdateStatus::Available,
        };
        DesktopSnapshot {
            activity: self.activity.clone(),
            dictation_shortcut: self.settings.dictation_hotkey.keycaps(),
            double_tap_lock: self.settings.double_tap_lock,
            double_tap_only: self.settings.double_tap_only,
            listener: None,
            operation_error: self.settings_load_error.clone(),
            transcription: DesktopTranscriptionSnapshot {
                downloaded_bytes: self.transcription_downloaded_bytes,
                error: self.transcription_picker_error.clone(),
                preparation_stage: None,
                selection: self.settings.transcription.clone(),
                preparing: self.transcription_downloading,
            },
            update_status,
        }
    }

    fn dispatch(&mut self, action: DesktopAction) -> color_eyre::Result<()> {
        match action {
            DesktopAction::ClearError => {}
            DesktopAction::RestartIntoUpdate
            | DesktopAction::StartListening
            | DesktopAction::StopListening => {
                return Err(color_eyre::eyre::eyre!(
                    "desktop action is unavailable on this host"
                ));
            }
            DesktopAction::SetDictationShortcut(shortcut) => {
                let modifiers = HotkeyModifiers {
                    control: shortcut.control.then_some(Default::default()),
                    option: shortcut.alt.then_some(Default::default()),
                    shift: shortcut.shift.then_some(Default::default()),
                    command: shortcut.platform.then_some(Default::default()),
                    function: shortcut.function,
                };
                let key = if shortcut.key.is_empty() {
                    None
                } else {
                    Some(
                        hotkey_key(&shortcut.key)
                            .map_err(|message| color_eyre::eyre::eyre!(message))?,
                    )
                };
                let binding = HotkeyBinding { modifiers, key };
                if binding.is_empty() {
                    return Err(color_eyre::eyre::eyre!("shortcut cannot be empty"));
                }
                if binding.key.is_some()
                    && binding.modifiers.is_empty()
                    && !binding
                        .key
                        .as_ref()
                        .is_some_and(|key| is_function_key(&key.label))
                {
                    return Err(color_eyre::eyre::eyre!("shortcut requires a modifier"));
                }
                if hotkey_binding_conflicts(&self.settings, HotkeyKind::Dictation, &binding) {
                    return Err(color_eyre::eyre::eyre!("shortcut is already in use"));
                }
                let mut candidate = self.settings.clone();
                candidate.dictation_hotkey = binding;
                if !self.preview {
                    candidate.save()?;
                }
                self.settings = candidate;
                self.settings_save_generation = self.settings_save_generation.wrapping_add(1);
                self.settings_dirty = false;
                self.settings_load_error = None;
            }
            DesktopAction::SetDoubleTapLock(enabled) => {
                let mut candidate = self.settings.clone();
                candidate.double_tap_lock = enabled;
                if !enabled {
                    candidate.double_tap_only = false;
                }
                if !self.preview {
                    candidate.save()?;
                }
                self.settings = candidate;
                self.settings_save_generation = self.settings_save_generation.wrapping_add(1);
                self.settings_dirty = false;
                self.settings_load_error = None;
                self.double_tap_toggle.set_enabled(enabled);
            }
            DesktopAction::SetDoubleTapOnly(enabled) => {
                let mut candidate = self.settings.clone();
                candidate.double_tap_only = enabled
                    && candidate.double_tap_lock
                    && candidate.dictation_hotkey.key.is_some();
                if !self.preview {
                    candidate.save()?;
                }
                self.settings = candidate;
                self.settings_save_generation = self.settings_save_generation.wrapping_add(1);
                self.settings_dirty = false;
                self.settings_load_error = None;
            }
        }
        Ok(())
    }
}

impl Render for AppWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model_notice = self.render_model_notice(cx);
        let content = match self.pane {
            Pane::HudLab => self.render_hud_lab(cx),
            Pane::Activity => self.render_activity(cx),
            Pane::History => self.render_history(cx),
            Pane::Modes => self.render_modes(window, cx),
            Pane::Settings => self.render_settings(window, cx),
        };
        let transcription_picker = self
            .transcription_picker_language
            .clone()
            .map(|language| self.render_transcription_picker(&language, cx));
        let setup = self.setup_visible.then(|| self.render_setup(cx));
        let mode_context_menu = (self.pane == Pane::Modes)
            .then(|| self.render_mode_context_menu(cx))
            .flatten();
        window_frame()
            .track_focus(&self.window_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && this.history_retention_open {
                    this.history_retention_open = false;
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                if event.keystroke.key == "escape" && this.mode_context_menu.take().is_some() {
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                if event.keystroke.key == "escape" && this.transcription_picker_language.is_some() {
                    cx.stop_propagation();
                    this.dismiss_transcription_picker(cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.mode_context_menu.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .on_action(|_: &CloseWindow, window, _| window.remove_window())
            .on_action(|_: &MinimizeWindow, window, _| window.minimize_window())
            .on_action(|_: &ToggleFullscreen, window, _| window.toggle_fullscreen())
            .on_action(cx.listener(|this, _: &ShowSettings, window, cx| {
                this.select_pane(Pane::Settings, cx);
                window.activate_window();
            }))
            .on_action(cx.listener(|this, _: &ShowModes, window, cx| {
                this.select_pane(Pane::Modes, cx);
                window.activate_window();
            }))
            .on_action(cx.listener(|this, _: &ShowHistory, window, cx| {
                this.select_pane(Pane::History, cx);
                window.activate_window();
            }))
            .child(self.render_navigation(cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .children(model_notice)
                    .child(div().flex_1().min_h_0().child(content)),
            )
            .children(setup)
            .children(transcription_picker)
            .children(mode_context_menu)
    }
}

/// Deterministic retained-history fixtures for the History pane preview.
fn preview_history() -> Option<History> {
    use crate::history::{HistoryDraft, HistoryProcessing, HistoryStore, now_ms};
    let directory =
        std::env::temp_dir().join(format!("hex-history-preview-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).ok()?;
    let mut store = HistoryStore::open(
        directory.join("history.json"),
        HistoryRetention::Week,
        now_ms(),
    );
    let now = now_ms();
    let fixtures = [
        (
            26 * 60 * 60 * 1_000,
            HistoryKind::Dictation,
            "so um the parser needs a retry when the socket drops",
            "The parser needs a retry when the socket drops.",
            Some("Zed"),
            Some(("Work notes", 940)),
        ),
        (
            3 * 60 * 60 * 1_000,
            HistoryKind::Send,
            "sounds good shipping it after lunch",
            "Sounds good — shipping it after lunch.",
            Some("Slack"),
            Some(("Messages", 610)),
        ),
        (
            4 * 60 * 1_000,
            HistoryKind::Dictation,
            "so um remember to publish the appcast after validating the artifact and uh \
             double check that the release notes mention the new capture placeholders \
             before anyone downloads it",
            "Remember to publish the appcast after validating the artifact, and double-check \
             that the release notes mention the new capture placeholders before anyone \
             downloads it.",
            Some("Messages"),
            None,
        ),
    ];
    for (age_ms, kind, raw, final_text, application, processing) in fixtures {
        let _ = store.record(
            HistoryDraft {
                kind,
                raw_text: raw.into(),
                final_text: final_text.into(),
                application: application.map(Into::into),
                processing: processing.map(|(profile, latency_ms)| HistoryProcessing {
                    profile: profile.into(),
                    latency_ms,
                    fallback: None,
                }),
                audio_ms: 2_800,
                inference_ms: 96,
                total_ms: 3_400,
            },
            now.saturating_sub(age_ms),
        );
    }
    Some(History::new(store))
}

fn hotkey_idle_width(keycap_count: usize) -> f32 {
    (112.0 + hotkey_keycaps_width(keycap_count)).max(HOTKEY_MIN_WIDTH)
}

const fn hotkey_kind_index(kind: HotkeyKind) -> usize {
    match kind {
        HotkeyKind::Dictation | HotkeyKind::DictationAlternative(_) => 0,
        HotkeyKind::PasteLast => 1,
    }
}

const fn hotkey_side_index(side: ModifierSide) -> usize {
    match side {
        ModifierSide::Left => 0,
        ModifierSide::Either => 1,
        ModifierSide::Right => 2,
    }
}

fn standalone_modifier_side(binding: &HotkeyBinding) -> Option<ModifierSide> {
    if binding.key.is_some() || binding.modifiers.count() != 1 {
        return None;
    }
    binding
        .modifiers
        .control
        .or(binding.modifiers.option)
        .or(binding.modifiers.shift)
        .or(binding.modifiers.command)
}

fn set_standalone_modifier_side(binding: &mut HotkeyBinding, side: ModifierSide) {
    if standalone_modifier_side(binding).is_none() {
        return;
    }
    if binding.modifiers.control.is_some() {
        binding.modifiers.control = Some(side);
    } else if binding.modifiers.option.is_some() {
        binding.modifiers.option = Some(side);
    } else if binding.modifiers.shift.is_some() {
        binding.modifiers.shift = Some(side);
    } else if binding.modifiers.command.is_some() {
        binding.modifiers.command = Some(side);
    }
}

fn hotkey_binding_conflicts(
    settings: &AppSettings,
    kind: HotkeyKind,
    binding: &HotkeyBinding,
) -> bool {
    let mut others = match kind {
        HotkeyKind::Dictation => Vec::new(),
        HotkeyKind::DictationAlternative(_) | HotkeyKind::PasteLast => {
            vec![settings.dictation_hotkey.clone()]
        }
    };
    others.extend(
        settings
            .dictation_hotkey_alternatives
            .iter()
            .enumerate()
            .filter(|(index, alternative)| {
                kind != HotkeyKind::DictationAlternative(*index) && !alternative.is_empty()
            })
            .map(|(_, alternative)| alternative.clone()),
    );
    if kind != HotkeyKind::PasteLast
        && let Some(paste) = &settings.paste_last_hotkey
    {
        others.push(paste.clone());
    }
    crate::app_settings::hotkey_conflicts(binding, others)
}

fn hotkey_side_binding(
    settings: &AppSettings,
    kind: HotkeyKind,
    side: ModifierSide,
) -> Option<HotkeyBinding> {
    let mut binding = match kind {
        HotkeyKind::Dictation => &settings.dictation_hotkey,
        HotkeyKind::DictationAlternative(index) => {
            settings.dictation_hotkey_alternatives.get(index)?
        }
        HotkeyKind::PasteLast => settings.paste_last_hotkey.as_ref()?,
    }
    .clone();
    standalone_modifier_side(&binding)?;
    set_standalone_modifier_side(&mut binding, side);
    (!hotkey_binding_conflicts(settings, kind, &binding)).then_some(binding)
}

fn hotkey_control_width(active: bool, idle_width: f32, animated_width: f32) -> f32 {
    if active { animated_width } else { idle_width }
}

fn hotkey_control_intensity(active: bool, intensity: f32) -> f32 {
    if active { intensity } else { 0.0 }
}

fn hotkey_capture_width(keycap_count: usize) -> f32 {
    if keycap_count == 0 {
        180.0
    } else {
        236.0 + hotkey_keycaps_width(keycap_count)
    }
}

fn hotkey_saved_width(keycap_count: usize) -> f32 {
    (53.0 + hotkey_keycaps_width(keycap_count)).max(HOTKEY_MIN_WIDTH)
}

fn hotkey_keycaps_width(keycap_count: usize) -> f32 {
    if keycap_count == 0 {
        0.0
    } else {
        34.0 * keycap_count as f32 + 3.0 * keycap_count.saturating_sub(1) as f32
    }
}

fn recording_audio_index(behavior: RecordingAudioBehavior) -> usize {
    match behavior {
        RecordingAudioBehavior::Mute => 0,
        RecordingAudioBehavior::PauseMedia => 1,
        RecordingAudioBehavior::DoNothing => 2,
    }
}

fn segmented_geometry(position: f32, widths: [f32; 3]) -> (f32, f32) {
    let position = position.clamp(0.0, 2.0);
    let lower = position.floor() as usize;
    let upper = (lower + 1).min(2);
    let progress = position - lower as f32;
    let lefts = [2.0, 2.0 + widths[0], 2.0 + widths[0] + widths[1]];
    (
        lefts[lower] + (lefts[upper] - lefts[lower]) * progress,
        widths[lower] + (widths[upper] - widths[lower]) * progress,
    )
}

fn sound_volume_index(settings: &AppSettings) -> usize {
    if !settings.sound_effects {
        return 0;
    }
    [0.25_f32, 0.5, 0.75, 1.0]
        .iter()
        .enumerate()
        .min_by(|(_, left), (_, right)| {
            (settings.sound_effect_volume - **left)
                .abs()
                .total_cmp(&(settings.sound_effect_volume - **right).abs())
        })
        .map_or(0, |(index, _)| index + 1)
}

fn advance_highlight(current: usize, direction: i32, count: usize) -> usize {
    if direction < 0 {
        current.saturating_sub(1)
    } else {
        (current + 1).min(count - 1)
    }
}

fn mode_row(
    title: impl Into<String>,
    subtitle: impl Into<String>,
    selected: bool,
    applications: &[String],
    browser_hosts: &[String],
    catalog: &ApplicationCatalogState,
) -> Div {
    let title = title.into();
    let subtitle = subtitle.into();
    let has_activations = !applications.is_empty() || !browser_hosts.is_empty();
    div()
        .w_full()
        .min_h(px(52.0))
        .px_3()
        .py_2()
        .flex()
        .flex_col()
        .items_start()
        .justify_center()
        .gap_1()
        .rounded(px(6.0))
        .when(selected, |row| {
            row.bg(rgb(0x292929)).hover(|row| row.bg(rgb(0x303030)))
        })
        .when(!selected, |row| row.hover(|row| row.bg(rgb(SURFACE_HOVER))))
        .child(
            div()
                .w_full()
                .text_size(px(12.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(TEXT))
                .truncate()
                .child(title),
        )
        .when(has_activations, |row| {
            row.child(mode_activation_icons(applications, browser_hosts, catalog))
        })
        .when(!has_activations, |row| {
            row.child(
                div()
                    .text_size(px(10.0))
                    .text_color(rgb(if selected { TEXT_SOFT } else { FAINT }))
                    .truncate()
                    .child(subtitle),
            )
        })
}

fn mode_activation_icons(
    applications: &[String],
    browser_hosts: &[String],
    catalog: &ApplicationCatalogState,
) -> AnyElement {
    let total = applications.len() + browser_hosts.len();
    let visible = total.min(5);
    let mut icons = Vec::with_capacity(visible + usize::from(total > visible));
    for name in applications.iter().take(visible) {
        let application = match catalog {
            ApplicationCatalogState::Loaded(applications) => applications
                .iter()
                .find(|application| &application.name == name),
            _ => None,
        };
        icons.push(application_icon(application, Some(name), 18.0));
    }
    for host in browser_hosts
        .iter()
        .take(visible.saturating_sub(icons.len()))
    {
        let initial = host
            .chars()
            .find(|character| character.is_alphanumeric())
            .map(|character| character.to_uppercase().to_string())
            .unwrap_or_else(|| "?".into());
        icons.push(
            div()
                .size(px(18.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .border_1()
                .border_color(rgb(0x444444))
                .bg(rgb(0xeeeeee))
                .text_size(px(8.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(0x202020))
                .child(initial)
                .into_any_element(),
        );
    }
    if total > visible {
        icons.push(
            div()
                .h(px(18.0))
                .px_1()
                .flex()
                .items_center()
                .rounded(px(4.0))
                .bg(rgb(0x303030))
                .text_size(px(8.0))
                .text_color(rgb(MUTED))
                .child(format!("+{}", total - visible))
                .into_any_element(),
        );
    }
    div()
        .h(px(18.0))
        .flex()
        .items_center()
        .gap_1()
        .children(icons)
        .into_any_element()
}

fn application_icon(
    application: Option<&InstalledApplication>,
    fallback_name: Option<&str>,
    size: f32,
) -> AnyElement {
    if let Some(icon) = application.and_then(|application| application.icon.as_ref()) {
        return img(icon.clone())
            .size(px(size))
            .rounded(px(size * 0.22))
            .into_any_element();
    }
    let initial = fallback_name
        .and_then(|name| name.chars().find(|character| character.is_alphanumeric()))
        .map(|character| character.to_uppercase().to_string())
        .unwrap_or_else(|| "?".into());
    div()
        .size(px(size))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(size * 0.22))
        .bg(rgb(0x343434))
        .text_size(px(size * 0.42))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(TEXT_SOFT))
        .child(initial)
        .into_any_element()
}

fn compact_mode_field(label: &'static str, control: impl IntoElement) -> Div {
    div()
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_size(px(9.0)).text_color(rgb(FAINT)).child(label))
        .child(control)
}

fn input_text(input: &ProcessingInput, cx: &App) -> String {
    input.entity.read(cx).text().trim().to_string()
}

fn browser_hosts(value: &str) -> Vec<String> {
    let mut hosts: Vec<_> = value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter_map(|value| {
            let candidate = if value.contains("://") {
                value.to_owned()
            } else {
                format!("https://{value}")
            };
            url::Url::parse(&candidate)
                .ok()?
                .host_str()
                .map(str::to_lowercase)
        })
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

fn apply_mode_inputs(inputs: &ModeInputs, mode: &mut DictationMode, is_default: bool, cx: &App) {
    mode.name = if is_default {
        "Global".into()
    } else {
        input_text(&inputs.name, cx)
    };
    mode.browser_hosts = if is_default {
        Vec::new()
    } else {
        browser_hosts(&input_text(&inputs.browser_hosts, cx))
    };
    for (inputs, replacement) in inputs.replacements.iter().zip(&mut mode.replacements) {
        replacement.matched_phrase = inputs.matched_phrase.entity.read(cx).text().to_string();
        replacement.output = inputs.output.entity.read(cx).text().to_string();
    }
}

fn dictation_model_notice(
    status: SetupStatus,
    preparing: bool,
) -> Option<(&'static str, &'static str)> {
    if status.transcription_model {
        return None;
    }
    Some(if preparing {
        (
            "Preparing speech model",
            "Dictation will be available once your local speech model is ready.",
        )
    } else {
        (
            "Download a speech model to start dictating",
            "The dictation shortcut needs a local speech model. Choose one to download first.",
        )
    })
}

fn setup_row(title: &'static str, description: &'static str, control: AnyElement) -> Div {
    div()
        .w_full()
        .min_h(px(70.0))
        .py_3()
        .flex()
        .items_center()
        .gap_4()
        .border_b_1()
        .border_color(rgb(LINE))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(settings_copy(title, description)),
        )
        .child(div().flex_shrink_0().child(control))
}

fn setup_group_label(label: &'static str) -> Div {
    div()
        .pb_2()
        .text_size(px(10.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(MUTED))
        .child(label)
}

fn setup_ready_badge() -> AnyElement {
    div()
        .h(px(28.0))
        .px_3()
        .flex()
        .items_center()
        .rounded_sm()
        .bg(rgb(0x17231a))
        .text_size(px(11.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(0x91bd99))
        .child("Ready")
        .into_any_element()
}

const fn permission_warning_copy(kind: PermissionKind) -> (&'static str, &'static str) {
    match kind {
        PermissionKind::Microphone => (
            "Microphone access is off",
            "HEX cannot record dictation until microphone access is restored.",
        ),
        PermissionKind::InputMonitoring => (
            "Input Monitoring is off",
            "HEX cannot recognize the dictation shortcut in other apps.",
        ),
        PermissionKind::Accessibility => (
            "Accessibility is off",
            "HEX cannot paste completed dictation into the foreground app.",
        ),
    }
}

const fn permission_action_label(action: PermissionAction) -> &'static str {
    match action {
        PermissionAction::RequestMicrophone => "Allow",
        PermissionAction::OpenMicrophoneSettings => "Open Settings",
        PermissionAction::OpenInputMonitoringSettings
        | PermissionAction::OpenAccessibilitySettings => "Grant Access",
    }
}

const fn permission_warning_id(kind: PermissionKind) -> &'static str {
    match kind {
        PermissionKind::Microphone => "permission-warning-microphone",
        PermissionKind::InputMonitoring => "permission-warning-input-monitoring",
        PermissionKind::Accessibility => "permission-warning-accessibility",
    }
}

fn hotkey_modifiers(modifiers: GpuiModifiers) -> HotkeyModifiers {
    hotkey_modifiers_with_flags(modifiers, crate::suppression::physical_modifier_flags())
}

fn hotkey_modifiers_with_flags(modifiers: GpuiModifiers, flags: u64) -> HotkeyModifiers {
    let physical = crate::app_settings::modifiers_from_flags(flags);
    if !physical.is_empty() {
        return physical;
    }
    HotkeyModifiers {
        control: modifiers.control.then_some(Default::default()),
        option: modifiers.alt.then_some(Default::default()),
        shift: modifiers.shift.then_some(Default::default()),
        command: modifiers.platform.then_some(Default::default()),
        function: modifiers.function,
    }
}

fn hotkey_key(key: &str) -> std::result::Result<HotkeyKey, &'static str> {
    let special = match key {
        "space" => Some((49, "Space")),
        "tab" => Some((48, "Tab")),
        "enter" => Some((36, "Return")),
        "backspace" => Some((51, "Delete")),
        "up" => Some((126, "Up")),
        "down" => Some((125, "Down")),
        "left" => Some((123, "Left")),
        "right" => Some((124, "Right")),
        "pageup" => Some((116, "Page Up")),
        "pagedown" => Some((121, "Page Down")),
        "home" => Some((115, "Home")),
        "end" => Some((119, "End")),
        "delete" => Some((117, "Forward Delete")),
        "insert" => Some((114, "Help")),
        "f1" => Some((122, "F1")),
        "f2" => Some((120, "F2")),
        "f3" => Some((99, "F3")),
        "f4" => Some((118, "F4")),
        "f5" => Some((96, "F5")),
        "f6" => Some((97, "F6")),
        "f7" => Some((98, "F7")),
        "f8" => Some((100, "F8")),
        "f9" => Some((101, "F9")),
        "f10" => Some((109, "F10")),
        "f11" => Some((103, "F11")),
        "f12" => Some((111, "F12")),
        "f13" => Some((105, "F13")),
        "f14" => Some((107, "F14")),
        "f15" => Some((113, "F15")),
        "f16" => Some((106, "F16")),
        "f17" => Some((64, "F17")),
        "f18" => Some((79, "F18")),
        "f19" => Some((80, "F19")),
        "f20" => Some((90, "F20")),
        _ => None,
    };
    if let Some((code, label)) = special {
        return Ok(HotkeyKey {
            code,
            label: label.into(),
        });
    }
    let mut characters = key.chars();
    let Some(character) = characters.next() else {
        return Err("Unsupported key");
    };
    if characters.next().is_some() {
        return Err("Unsupported key");
    }
    let code = crate::keyboard::key_code_for(character).map_err(|_| "Unsupported key")?;
    Ok(HotkeyKey {
        code,
        label: character.to_uppercase().collect(),
    })
}

fn is_function_key(label: &str) -> bool {
    label
        .strip_prefix('F')
        .and_then(|number| number.parse::<u8>().ok())
        .is_some_and(|number| (1..=20).contains(&number))
}

fn detail_placeholder(message: &'static str) -> AnyElement {
    div()
        .flex_1()
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(12.0))
        .text_color(rgb(FAINT))
        .child(message)
        .into_any_element()
}

fn detail_row(label: &'static str, value: impl Into<String>) -> AnyElement {
    div()
        .py_3()
        .flex()
        .items_start()
        .gap_4()
        .border_b_1()
        .border_color(rgb(LINE))
        .child(
            div()
                .w(px(92.0))
                .flex_none()
                .text_size(px(11.0))
                .text_color(rgb(FAINT))
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(12.0))
                .line_height(px(18.0))
                .text_color(rgb(TEXT_SOFT))
                .child(value.into()),
        )
        .into_any_element()
}

fn activity_context(reader: &EventReader) -> (Option<String>, Option<String>) {
    reader
        .current_context()
        .map(|(application, browser_url)| {
            (
                application.clone(),
                browser_url
                    .as_deref()
                    .and_then(|url| url::Url::parse(url).ok())
                    .and_then(|url| url.host_str().map(str::to_owned)),
            )
        })
        .unwrap_or_default()
}

fn event_age(timestamp_ms: u64) -> String {
    let seconds = now_ms().saturating_sub(timestamp_ms) / 1_000;
    match seconds {
        0..=59 => format!("{seconds}s ago"),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

fn event_summary(event: &VoiceEvent) -> String {
    match event {
        VoiceEvent::SessionStarted { .. } => "HEX session started".into(),
        VoiceEvent::State { state, device, .. } => match state {
            VoiceState::Sleeping => "Voice commands went to sleep".into(),
            VoiceState::Listening => format!("Voice commands resumed on {device}"),
            VoiceState::Dictating => format!("Dictation started on {device}"),
            VoiceState::Transcribing => "Dictation is being transcribed".into(),
            VoiceState::Stopping => "HEX is stopping".into(),
        },
        VoiceEvent::Transcript {
            phase,
            latency_ms,
            text,
            ..
        } => match phase {
            TranscriptPhase::Started => "Speech recognition started".into(),
            TranscriptPhase::Updated => format!("Heard \"{text}\" so far"),
            TranscriptPhase::Completed => format!("Heard \"{text}\" in {latency_ms} ms"),
        },
        VoiceEvent::Command {
            heard,
            command,
            outcome,
            ..
        } => {
            let command = command.as_deref().unwrap_or("an unmatched command");
            match outcome {
                CommandOutcome::Ignored => format!("Ignored \"{heard}\""),
                CommandOutcome::Woke => format!("\"{heard}\" woke HEX"),
                CommandOutcome::Slept => format!("\"{heard}\" put voice commands to sleep"),
                CommandOutcome::Submitted => format!("Sent {command} for \"{heard}\""),
                CommandOutcome::Executed => format!("Ran {command} for \"{heard}\""),
                CommandOutcome::Failed(error) => format!("Could not run {command}: {error}"),
            }
        }
        VoiceEvent::Dictation {
            phase,
            text,
            processing,
            ..
        } => {
            let description = match phase {
                DictationPhase::Started => "Dictation started".into(),
                DictationPhase::Discarded => "A brief dictation was discarded".into(),
                DictationPhase::Cancelled => "Dictation was cancelled".into(),
                DictationPhase::Transcribing => "Dictation is being transcribed".into(),
                DictationPhase::Pasted => text_or("Dictation was pasted", text),
                DictationPhase::Edited => text_or("Selected text was edited", text),
                DictationPhase::VoiceAction => text_or("Voice Action completed", text),
                DictationPhase::Logged => text_or("Journal entry was saved", text),
                DictationPhase::Repasted => text_or("The last dictation was pasted again", text),
                DictationPhase::Failed(error) => format!("Dictation failed: {error}"),
            };
            processing
                .as_ref()
                .map_or(description.clone(), |processing| {
                    processing.fallback.as_ref().map_or_else(
                        || {
                            format!(
                                "{description} · {} profile · {} ms",
                                processing.profile, processing.latency_ms
                            )
                        },
                        |error| {
                            format!(
                                "{description} · {} profile fell back to raw after {} ms: {}",
                                processing.profile, processing.latency_ms, error
                            )
                        },
                    )
                })
        }
        VoiceEvent::Context {
            application,
            browser_url,
            ..
        } => match (application, browser_url) {
            (Some(application), Some(url)) => format!("Context changed to {application} / {url}"),
            (Some(application), None) => format!("Context changed to {application}"),
            _ => "Foreground context became unavailable".into(),
        },
        VoiceEvent::ApiServerStarted { port, .. } => {
            format!("Local API started on port {port}")
        }
        VoiceEvent::ApiServerStopped { .. } => "Local API stopped".into(),
        VoiceEvent::ApiAuthFailed { method, path, .. } => {
            format!("Rejected unauthenticated {method} {path}")
        }
    }
}

fn text_or(prefix: &str, text: &str) -> String {
    if text.is_empty() {
        prefix.into()
    } else {
        format!("{prefix}: \"{text}\"")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[gpui::test]
    fn settings_content_stays_inside_the_pane_at_supported_widths(cx: &mut gpui::TestAppContext) {
        struct SettingsLayout;

        impl Render for SettingsLayout {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                window_frame()
                    .child(sidebar_frame().w(px(SIDEBAR_WIDTH)))
                    .child(
                        div().flex_1().min_w_0().h_full().child(settings_pane(
                            div()
                                .child(settings_panel().child(settings_row(
                                    "Microphone",
                                    "Automatically chooses the preferred available input",
                                    disclosure_button("Automatic"),
                                )))
                                .child(settings_copy(
                                    "Release the microphone when idle?",
                                    "HEX will open the microphone when you press the shortcut. This adds a start-up delay and removes pre-roll, so the beginning of speech may be missed if you speak right away.",
                                )),
                        )),
                    )
            }
        }

        let (_, cx) = cx.add_window_view(|_, _| SettingsLayout);
        for width in [MINIMUM_WIDTH, WINDOW_WIDTH, 1400.0] {
            cx.simulate_resize(size(px(width), px(MINIMUM_HEIGHT)));
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            let column = cx
                .debug_bounds("settings-content")
                .expect("content was rendered");
            let available = width - SIDEBAR_WIDTH - 64.0;
            let expected_width = available.min(PANE_CONTENT_WIDTH);
            assert_eq!(column.size.width, px(expected_width));
            assert_eq!(
                column.origin.x,
                px(SIDEBAR_WIDTH + 32.0 + (available - expected_width) / 2.0)
            );
        }
    }

    #[test]
    fn opening_the_app_prioritizes_settings_and_missing_permissions() {
        assert_eq!(Pane::default(), Pane::Settings);
        let ready = SetupStatus {
            microphone: PermissionState::Ready,
            input_monitoring: PermissionState::Ready,
            accessibility: PermissionState::Ready,
            transcription_model: true,
        };
        assert_eq!(Pane::Modes.on_reopen(ready), Pane::Modes);
        for status in [
            SetupStatus {
                microphone: PermissionState::NeedsRequest,
                ..ready
            },
            SetupStatus {
                input_monitoring: PermissionState::NeedsSettings,
                ..ready
            },
            SetupStatus {
                accessibility: PermissionState::NeedsSettings,
                ..ready
            },
        ] {
            assert_eq!(Pane::Modes.on_reopen(status), Pane::Settings);
            assert_eq!(Pane::Settings.on_reopen(status), Pane::Settings);
        }
        assert_eq!(Pane::from_preview(PreviewPane::Modes), Pane::Modes);
    }

    #[test]
    fn missing_dictation_model_has_a_notice_even_with_all_permissions_granted() {
        let mut status = SetupStatus {
            microphone: PermissionState::Ready,
            input_monitoring: PermissionState::Ready,
            accessibility: PermissionState::Ready,
            transcription_model: false,
        };
        let (title, description) = dictation_model_notice(status, false).unwrap();
        assert_eq!(title, "Download a speech model to start dictating");
        assert!(description.contains("dictation shortcut"));
        assert!(!status.ready());

        assert_eq!(
            dictation_model_notice(status, true).unwrap().0,
            "Preparing speech model"
        );
        // A failed or cancelled preparation must expose the download action again.
        assert_eq!(dictation_model_notice(status, false).unwrap().0, title);

        status.transcription_model = true;
        assert!(dictation_model_notice(status, false).is_none());
        assert!(dictation_model_notice(status, true).is_none());

        // Permission failures have their own setup UI.
        status.microphone = PermissionState::NeedsRequest;
        assert!(dictation_model_notice(status, false).is_none());
    }

    #[test]
    fn sidebar_update_button_only_appears_for_an_available_update() {
        use crate::sparkle::UpdateStatus;

        for (status, visible) in [
            (UpdateStatus::Unavailable, false),
            (UpdateStatus::Idle, false),
            (UpdateStatus::Checking, false),
            (UpdateStatus::UpToDate, false),
            (UpdateStatus::UpdateAvailable, true),
        ] {
            assert_eq!(sidebar_update_button(status, false).is_some(), visible);
        }
    }

    #[gpui::test]
    fn sidebar_update_click_dispatches_only_outside_preview(cx: &mut gpui::TestAppContext) {
        struct UpdateButton {
            preview: bool,
        }

        impl Render for UpdateButton {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                sidebar_update_button(crate::sparkle::UpdateStatus::UpdateAvailable, self.preview)
                    .unwrap()
                    .debug_selector(|| "sidebar-update".into())
            }
        }

        let calls = Rc::new(std::cell::Cell::new(0));
        cx.update(|cx| {
            let calls = calls.clone();
            cx.on_action(move |_: &CheckForUpdates, _| calls.set(calls.get() + 1));
        });

        for preview in [true, false] {
            let (_, cx) = cx.add_window_view(|_, _| UpdateButton { preview });
            cx.update(|window, cx| {
                // Test windows do not activate automatically; an inactive window masks the bug.
                window.activate_window();
                assert!(cx.active_window() == Some(window.window_handle()));
            });
            cx.run_until_parked();
            let button = cx.debug_bounds("sidebar-update").unwrap();
            cx.simulate_click(button.center(), GpuiModifiers::default());
            assert_eq!(calls.get(), usize::from(!preview), "preview={preview}");
        }
    }

    #[test]
    fn inactive_hotkey_controls_keep_their_idle_width() {
        assert_eq!(hotkey_control_width(false, 146.0, 270.0), 146.0);
        assert_eq!(hotkey_control_width(true, 146.0, 270.0), 270.0);
        assert_eq!(hotkey_control_intensity(false, 0.8), 0.0);
        assert_eq!(hotkey_control_intensity(true, 0.8), 0.8);
    }

    #[test]
    fn shortcut_side_changes_reject_overlap_without_changing_the_saved_binding() {
        let mut settings = AppSettings::default();
        settings.dictation_hotkey.modifiers.option = Some(ModifierSide::Left);
        settings.paste_last_hotkey = Some(HotkeyBinding {
            modifiers: HotkeyModifiers {
                option: Some(ModifierSide::Right),
                ..Default::default()
            },
            key: None,
        });
        let original = settings.dictation_hotkey.clone();

        assert!(
            hotkey_side_binding(&settings, HotkeyKind::Dictation, ModifierSide::Right).is_none()
        );
        assert!(
            hotkey_side_binding(&settings, HotkeyKind::Dictation, ModifierSide::Either).is_none()
        );
        assert_eq!(settings.dictation_hotkey, original);
        assert_eq!(
            hotkey_side_binding(&settings, HotkeyKind::Dictation, ModifierSide::Left),
            Some(original)
        );

        settings.paste_last_hotkey = Some(HotkeyBinding::paste_last_default());
        let candidate =
            hotkey_side_binding(&settings, HotkeyKind::Dictation, ModifierSide::Right).unwrap();
        assert_eq!(
            standalone_modifier_side(&candidate),
            Some(ModifierSide::Right)
        );
    }

    #[test]
    fn side_selection_only_changes_a_standalone_modifier() {
        let mut standalone = HotkeyBinding {
            modifiers: HotkeyModifiers {
                option: Some(ModifierSide::Left),
                ..Default::default()
            },
            key: None,
        };
        set_standalone_modifier_side(&mut standalone, ModifierSide::Either);
        assert_eq!(
            standalone_modifier_side(&standalone),
            Some(ModifierSide::Either)
        );

        let mut chord = HotkeyBinding {
            modifiers: HotkeyModifiers {
                option: Some(ModifierSide::Left),
                ..Default::default()
            },
            key: Some(HotkeyKey {
                code: 0,
                label: "A".into(),
            }),
        };
        set_standalone_modifier_side(&mut chord, ModifierSide::Right);
        assert_eq!(chord.modifiers.option, Some(ModifierSide::Left));
    }

    #[test]
    fn production_navigation_shows_only_shipped_panes() {
        assert_eq!(
            Pane::all(DesktopCapabilities::macos()),
            vec![Pane::Settings, Pane::Modes, Pane::History]
        );
    }

    #[test]
    fn application_catalog_loads_only_when_explicitly_requested() {
        let loading = ApplicationCatalogState::Loading;
        let loaded = ApplicationCatalogState::Loaded(Vec::new());

        assert!(application_catalog_load_needed(false, &loading, false));
        assert!(!application_catalog_load_needed(false, &loading, true));
        assert!(!application_catalog_load_needed(false, &loaded, false));
        assert!(!application_catalog_load_needed(true, &loading, false));
    }

    #[test]
    fn hotkey_capture_preserves_the_function_modifier() {
        let modifiers = hotkey_modifiers_with_flags(
            GpuiModifiers {
                function: true,
                ..Default::default()
            },
            0,
        );

        assert!(modifiers.function);
        assert_eq!(modifiers.count(), 1);
    }

    #[test]
    fn toggle_spring_is_frame_rate_independent_and_settles() {
        let simulate = |frame_rate: u32| {
            let mut spring = ToggleSpring::new(false);
            spring.set_enabled(true);
            for _ in 0..frame_rate {
                spring.advance(Duration::from_secs_f32(1.0 / frame_rate as f32));
            }
            spring
        };

        let at_60_hz = simulate(60);
        let at_120_hz = simulate(120);
        assert!(at_60_hz.is_settled());
        assert!(at_120_hz.is_settled());
        assert!((at_60_hz.position - at_120_hz.position).abs() < 0.001);
        assert_eq!(at_60_hz.position, 1.0);
    }

    #[test]
    fn recent_activity_keeps_context_older_than_the_visible_window() {
        let path = std::env::temp_dir().join(format!(
            "voice-control-app-window-{}-{}.ndjson",
            std::process::id(),
            crate::events::now_ms()
        ));
        let mut lines = vec![
            serde_json::to_string(&VoiceEvent::Context {
                timestamp_ms: 1,
                application: Some("Brave Browser".into()),
                browser_url: Some("https://meet.google.com/example".into()),
            })
            .unwrap(),
        ];
        lines.extend((0..=ACTIVITY_LIMIT).map(|timestamp_ms| {
            serde_json::to_string(&VoiceEvent::State {
                timestamp_ms: timestamp_ms as u64 + 2,
                state: VoiceState::Listening,
                device: "Test microphone".into(),
            })
            .unwrap()
        }));
        fs::write(&path, lines.join("\n")).unwrap();

        let mut reader = EventReader::open(&path);
        reader.refresh().unwrap();

        assert_eq!(reader.recent(ACTIVITY_LIMIT).len(), ACTIVITY_LIMIT);
        assert_eq!(
            activity_context(&reader),
            (Some("Brave Browser".into()), Some("meet.google.com".into()))
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn browser_hosts_are_canonicalized_and_deduplicated() {
        assert_eq!(
            browser_hosts(" https://www.Reddit.com/r/rust, , reddit.com, github.com/path"),
            ["github.com", "reddit.com", "www.reddit.com"]
        );
    }

    #[test]
    fn picker_arrows_stop_at_the_ends() {
        assert_eq!(advance_highlight(0, -1, 4), 0);
        assert_eq!(advance_highlight(2, 1, 4), 3);
        assert_eq!(advance_highlight(3, 1, 4), 3);
    }

    #[test]
    fn selected_transcription_model_is_not_active_until_installed() {
        let selection = TranscriptionSelection::default();
        let model = definition(selection.model);

        assert!(!transcription_selection_is_active(
            &selection,
            model,
            &selection.language,
            false
        ));
        assert!(transcription_selection_is_active(
            &selection,
            model,
            &selection.language,
            true
        ));
    }
}
