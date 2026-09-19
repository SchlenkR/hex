use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use color_eyre::eyre::{Result, WrapErr, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptionModelId {
    #[default]
    ParakeetV3,
    ParakeetPrimeline,
    AppleSpeech,
}

impl TranscriptionModelId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ParakeetV3 => "parakeet_v3",
            Self::ParakeetPrimeline => "parakeet_primeline",
            Self::AppleSpeech => "apple_speech",
        }
    }
}

impl FromStr for TranscriptionModelId {
    type Err = color_eyre::Report;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "parakeet_v3" => Ok(Self::ParakeetV3),
            "parakeet_primeline" => Ok(Self::ParakeetPrimeline),
            "apple_speech" => Ok(Self::AppleSpeech),
            _ => bail!("unknown transcription model: {value}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub enum ModelPreparationStage {
    Downloading,
    Verifying,
    Loading,
}

impl ModelPreparationStage {
    pub fn store(self, stage: &AtomicU8) {
        stage.store(self as u8, Ordering::Release);
    }

    pub fn load(stage: &AtomicU8) -> Self {
        match stage.load(Ordering::Acquire) {
            value if value == Self::Downloading as u8 => Self::Downloading,
            value if value == Self::Verifying as u8 => Self::Verifying,
            value if value == Self::Loading as u8 => Self::Loading,
            value => panic!("invalid model preparation stage {value}"),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct VerificationReceipt {
    bytes: u64,
    sha256: String,
    modified_ns: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct TranscriptionSelection {
    pub model: TranscriptionModelId,
    pub language: String,
    pub recognition_hints: String,
}

impl Default for TranscriptionSelection {
    fn default() -> Self {
        Self {
            model: TranscriptionModelId::default(),
            language: "en".into(),
            recognition_hints: String::new(),
        }
    }
}

#[derive(Clone, Copy)]
pub struct ModelChoice {
    pub model: &'static ModelDefinition,
}

pub struct ModelDefinition {
    pub id: TranscriptionModelId,
    pub name: &'static str,
    pub realtime: &'static str,
    pub realtime_context: &'static str,
    pub quality: &'static str,
    pub quality_context: &'static str,
    pub coverage: &'static str,
    pub timestamps: &'static str,
    pub runtime: ModelRuntime,
    pub languages: &'static [&'static str],
    pub accepts_language_hint: bool,
    pub supports_language_detection: bool,
    pub supports_recognition_hints: bool,
}

#[derive(Clone, Copy)]
pub enum ModelRuntime {
    Gguf(&'static GgufArtifact),
    AppleSpeech,
}

pub struct GgufArtifact {
    pub filename: &'static str,
    pub revision: &'static str,
    pub repository: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
    pub architecture: &'static str,
    pub variant: &'static str,
}

impl ModelDefinition {
    pub const fn available(&self) -> bool {
        !matches!(self.runtime, ModelRuntime::AppleSpeech)
    }

    fn download_url(&self) -> Result<String> {
        let ModelRuntime::Gguf(artifact) = self.runtime else {
            bail!("{} is managed by macOS and has no GGUF artifact", self.name);
        };
        Ok(format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            artifact.repository, artifact.revision, artifact.filename
        ))
    }

    pub fn supports_language(&self, language: &str) -> bool {
        if language == AUTO_LANGUAGE {
            self.supports_language_detection
        } else {
            self.languages.contains(&"*") || self.languages.contains(&language)
        }
    }

    pub fn runtime_language<'a>(&self, language: &'a str) -> &'a str {
        language
    }

    pub fn runtime_language_hint<'a>(&self, language: &'a str) -> Option<&'a str> {
        (language != AUTO_LANGUAGE && self.accepts_language_hint)
            .then(|| self.runtime_language(language))
    }

    pub fn presentation_languages(&self) -> Vec<&'static str> {
        self.languages
            .iter()
            .copied()
            .chain(self.supports_language_detection.then_some(AUTO_LANGUAGE))
            .collect()
    }

    pub fn size_label(&self) -> String {
        match self.runtime {
            ModelRuntime::AppleSpeech => "Managed".into(),
            ModelRuntime::Gguf(artifact) if artifact.bytes >= 1_000_000_000 => {
                format!("{:.1} GB", artifact.bytes as f64 / 1_000_000_000.0)
            }
            ModelRuntime::Gguf(artifact) => format!("{} MB", artifact.bytes / 1_000_000),
        }
    }

    pub const fn download_bytes(&self) -> Option<u64> {
        match self.runtime {
            ModelRuntime::Gguf(artifact) => Some(artifact.bytes),
            ModelRuntime::AppleSpeech => None,
        }
    }
}

const PARAKEET_V3_LANGUAGES: &[&str] = &[
    "bg", "hr", "cs", "da", "nl", "en", "et", "fi", "fr", "de", "el", "hu", "it", "lv", "lt", "mt",
    "pl", "pt", "ro", "ru", "sk", "sl", "es", "sv", "uk",
];

const PARAKEET_V3_ARTIFACT: GgufArtifact = GgufArtifact {
    filename: "parakeet-tdt-0.6b-v3-Q8_0.gguf",
    revision: "85ac09ea12fc4b1112fa76810059364bc6adc9de",
    repository: "handy-computer/parakeet-tdt-0.6b-v3-gguf",
    bytes: 739_508_576,
    sha256: "5859f77944efcd8eafa23a6350731960b2b55b2203df51f319665c807d802cc7",
    architecture: "parakeet",
    variant: "tdt-0.6b-v3",
};

const PARAKEET_PRIMELINE_ARTIFACT: GgufArtifact = GgufArtifact {
    filename: "parakeet-primeline-Q8_0.gguf",
    revision: "7c63545b352e5d410ccc1ea1eb49aa751e65780b",
    repository: "handy-computer/parakeet-primeline-gguf",
    bytes: 739_508_640,
    sha256: "dbd1ec2985aea64d16550a0068ea9153c0bc01955bf2a5c18b74ca213546d3cd",
    architecture: "parakeet",
    variant: "tdt-0.6b-primeline",
};

pub const MODELS: &[ModelDefinition] = &[
    ModelDefinition {
        id: TranscriptionModelId::ParakeetV3,
        name: "Parakeet v3",
        realtime: "~33x",
        realtime_context: "local speed · M2 Max",
        quality: "1.94%",
        quality_context: "English benchmark",
        coverage: "25 languages",
        timestamps: "Token timestamps",
        runtime: ModelRuntime::Gguf(&PARAKEET_V3_ARTIFACT),
        languages: PARAKEET_V3_LANGUAGES,
        // The pinned v3 artifact detects language from audio and has no prompt input.
        accepts_language_hint: false,
        supports_language_detection: false,
        supports_recognition_hints: false,
    },
    ModelDefinition {
        id: TranscriptionModelId::ParakeetPrimeline,
        name: "Parakeet v3 Primeline (Deutsch)",
        realtime: "~33x",
        realtime_context: "local speed · M2 Max",
        quality: "5.98%",
        quality_context: "German FLEURS benchmark",
        coverage: "German fine-tune",
        timestamps: "Token timestamps",
        runtime: ModelRuntime::Gguf(&PARAKEET_PRIMELINE_ARTIFACT),
        languages: PARAKEET_V3_LANGUAGES,
        accepts_language_hint: false,
        supports_language_detection: false,
        supports_recognition_hints: false,
    },
    ModelDefinition {
        id: TranscriptionModelId::AppleSpeech,
        name: "Apple Speech",
        realtime: "On device",
        realtime_context: "macOS 26",
        quality: "System",
        quality_context: "Apple managed",
        coverage: "System locales",
        timestamps: "Segment timestamps",
        runtime: ModelRuntime::AppleSpeech,
        languages: &["*"],
        accepts_language_hint: true,
        supports_language_detection: false,
        supports_recognition_hints: false,
    },
];

pub const AUTO_LANGUAGE: &str = "auto";

pub const LANGUAGES: &[(&str, &str)] = &[
    (AUTO_LANGUAGE, "Auto"),
    ("en", "English"),
    ("zh", "Mandarin Chinese"),
    ("yue", "Cantonese"),
    ("ja", "Japanese"),
    ("ko", "Korean"),
    ("es", "Spanish"),
    ("fr", "French"),
    ("de", "German"),
    ("pt", "Portuguese"),
    ("it", "Italian"),
    ("ar", "Arabic"),
    ("hi", "Hindi"),
    ("vi", "Vietnamese"),
    ("ru", "Russian"),
    ("uk", "Ukrainian"),
    ("nl", "Dutch"),
    ("pl", "Polish"),
    ("tr", "Turkish"),
    ("sv", "Swedish"),
    ("da", "Danish"),
    ("fi", "Finnish"),
    ("cs", "Czech"),
    ("el", "Greek"),
    ("ro", "Romanian"),
    ("hu", "Hungarian"),
    ("id", "Indonesian"),
    ("ms", "Malay"),
    ("th", "Thai"),
    ("fa", "Persian"),
    ("bg", "Bulgarian"),
    ("hr", "Croatian"),
    ("et", "Estonian"),
    ("lv", "Latvian"),
    ("lt", "Lithuanian"),
    ("mt", "Maltese"),
    ("sk", "Slovak"),
    ("sl", "Slovenian"),
    ("mk", "Macedonian"),
    ("fil", "Filipino"),
];

pub fn definition(id: TranscriptionModelId) -> &'static ModelDefinition {
    MODELS
        .iter()
        .find(|model| model.id == id)
        .expect("every transcription model id has a definition")
}

pub fn language_name(code: &str) -> &str {
    LANGUAGES
        .iter()
        .find_map(|(candidate, name)| (*candidate == code).then_some(*name))
        .unwrap_or(code)
}

pub(crate) fn choices_for_runtime(language: &str) -> Vec<ModelChoice> {
    MODELS
        .iter()
        .filter(|model| model.supports_language(language))
        .map(|model| ModelChoice { model })
        .collect()
}

pub fn validate(selection: &TranscriptionSelection) -> Result<&'static ModelDefinition> {
    if !LANGUAGES
        .iter()
        .any(|(code, _)| *code == selection.language)
    {
        bail!("unsupported transcription language: {}", selection.language);
    }
    let model = definition(selection.model);
    if !model.available() {
        bail!("{} is unavailable", model.name);
    }
    if !model.supports_language(&selection.language) {
        bail!(
            "{} does not support {}",
            model.name,
            language_name(&selection.language)
        );
    }
    if !model.supports_recognition_hints && !selection.recognition_hints.trim().is_empty() {
        bail!("{} does not support recognition hints", model.name);
    }
    Ok(model)
}

static MODELS_DIR_OVERRIDE: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();

pub fn set_models_dir(path: Option<&str>) {
    let path = path
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    *MODELS_DIR_OVERRIDE
        .get_or_init(Default::default)
        .write()
        .unwrap_or_else(|error| error.into_inner()) = path;
}

pub fn models_dir() -> Result<PathBuf> {
    let override_dir = MODELS_DIR_OVERRIDE
        .get_or_init(Default::default)
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    match override_dir {
        Some(path) => Ok(path),
        None => Ok(crate::app_paths::support_dir()?.join("models")),
    }
}

pub fn max_audio_chunk_samples(architecture: &str, max_audio_ms: i64) -> Option<usize> {
    // Cohere's encoder capacity is not its recommended transcription window.
    // The pinned model's reference processor chunks long audio at 35 seconds.
    let maximum_ms = match (architecture, max_audio_ms) {
        ("cohere_asr", limit) if limit > 0 => limit.min(35_000),
        ("cohere_asr", _) => 35_000,
        (_, limit) => limit,
    };
    usize::try_from(maximum_ms)
        .ok()
        .filter(|milliseconds| *milliseconds > 0)?
        .checked_mul(16)
}

pub fn model_path(model: &ModelDefinition) -> Result<PathBuf> {
    let ModelRuntime::Gguf(artifact) = model.runtime else {
        bail!("{} is managed by macOS and has no model path", model.name);
    };
    Ok(models_dir()?.join(artifact.filename))
}

pub fn is_installed(model: &ModelDefinition, language: &str) -> bool {
    #[cfg(not(target_os = "macos"))]
    let _ = language;
    match model.runtime {
        ModelRuntime::Gguf(artifact) => model_path(model)
            .and_then(|path| Ok(fs::metadata(path)?.len() == artifact.bytes))
            .unwrap_or(false),
        #[cfg(target_os = "macos")]
        ModelRuntime::AppleSpeech => crate::apple_speech::AppleSpeech::is_ready(language),
        #[cfg(not(target_os = "macos"))]
        ModelRuntime::AppleSpeech => false,
    }
}

pub fn is_verified(model: &ModelDefinition) -> bool {
    let Ok(path) = model_path(model) else {
        return false;
    };
    verification_receipt_matches(&path, model)
}

#[cfg(target_os = "macos")]
pub fn verify_installed(model: &ModelDefinition, canceled: &AtomicBool) -> Result<PathBuf> {
    check_canceled(canceled)?;
    let path = model_path(model)?;
    verify_installed_at(&path, model, canceled)?;
    Ok(path)
}

#[cfg(target_os = "macos")]
fn verify_installed_at(path: &Path, model: &ModelDefinition, canceled: &AtomicBool) -> Result<()> {
    check_canceled(canceled)?;
    if !path.is_file() {
        bail!(
            "{} is no longer installed. Open Settings to download it.",
            model.name
        );
    }
    let _lock = lock_downloads(path.parent().expect("model directory"), canceled)?;
    if !verification_receipt_matches(path, model) {
        verify_file_with_cancel(path, model, canceled)?;
        write_verification_receipt(path, model)?;
    }
    Ok(())
}

fn lock_downloads(directory: &Path, canceled: &AtomicBool) -> Result<File> {
    let lock = File::create(directory.join(".download.lock"))?;
    loop {
        check_canceled(canceled)?;
        match lock.try_lock_exclusive() {
            Ok(()) => return Ok(lock),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn verification_receipt_matches(path: &Path, model: &ModelDefinition) -> bool {
    let ModelRuntime::Gguf(artifact) = model.runtime else {
        return false;
    };
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    let Ok(receipt) = fs::read(verification_receipt_path(path))
        .and_then(|data| serde_json::from_slice::<VerificationReceipt>(&data).map_err(Into::into))
    else {
        return false;
    };
    metadata.len() == artifact.bytes
        && receipt.bytes == artifact.bytes
        && receipt.sha256 == artifact.sha256
        && modified_ns(&metadata).is_some_and(|modified| modified == receipt.modified_ns)
}

pub fn download_with_progress(
    model: &ModelDefinition,
    canceled: &AtomicBool,
    downloaded_bytes: &AtomicU64,
) -> Result<PathBuf> {
    download_with_stage_progress(
        model,
        canceled,
        downloaded_bytes,
        &AtomicU8::new(ModelPreparationStage::Downloading as u8),
    )
}

pub fn download_with_stage_progress(
    model: &ModelDefinition,
    canceled: &AtomicBool,
    downloaded_bytes: &AtomicU64,
    stage: &AtomicU8,
) -> Result<PathBuf> {
    let ModelRuntime::Gguf(artifact) = model.runtime else {
        bail!("{} is installed and managed by macOS", model.name);
    };
    ModelPreparationStage::Downloading.store(stage);
    check_canceled(canceled)?;
    let directory = models_dir()?;
    fs::create_dir_all(&directory)?;
    let _lock = lock_downloads(&directory, canceled)?;
    check_canceled(canceled)?;
    let destination = directory.join(artifact.filename);
    if destination.exists() {
        if is_verified(model) {
            downloaded_bytes.store(artifact.bytes, Ordering::Relaxed);
            return Ok(destination);
        }
        ModelPreparationStage::Verifying.store(stage);
        match verify_file_with_cancel(&destination, model, canceled) {
            Ok(()) => {
                write_verification_receipt(&destination, model)?;
                downloaded_bytes.store(artifact.bytes, Ordering::Relaxed);
                return Ok(destination);
            }
            Err(error) if canceled.load(Ordering::Relaxed) => return Err(error),
            Err(error) => {
                tracing::warn!(%error, path = %destination.display(), "will replace invalid transcription model after verifying its replacement");
            }
        }
    }
    let partial = destination.with_extension("gguf.partial");
    if fs::metadata(&partial).is_ok_and(|metadata| metadata.len() > artifact.bytes) {
        fs::remove_file(&partial)?;
    }
    downloaded_bytes.store(
        fs::metadata(&partial).map_or(0, |metadata| metadata.len()),
        Ordering::Relaxed,
    );
    if fs::metadata(&partial).is_ok_and(|metadata| metadata.len() == artifact.bytes) {
        ModelPreparationStage::Verifying.store(stage);
        match verify_file_with_cancel(&partial, model, canceled) {
            Ok(()) => {
                File::open(&partial)?.sync_all()?;
                fs::rename(&partial, &destination)?;
                File::open(&directory)?.sync_all()?;
                write_verification_receipt(&destination, model)?;
                downloaded_bytes.store(artifact.bytes, Ordering::Relaxed);
                return Ok(destination);
            }
            Err(error) if canceled.load(Ordering::Relaxed) => return Err(error),
            Err(error) => {
                tracing::warn!(%error, path = %partial.display(), "replacing invalid partial transcription model");
                fs::remove_file(&partial)?;
                downloaded_bytes.store(0, Ordering::Relaxed);
            }
        }
    }
    ModelPreparationStage::Downloading.store(stage);
    let mut child = Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--retry",
            "3",
            "--connect-timeout",
            "15",
            "--speed-limit",
            "1024",
            "--speed-time",
            "60",
            "--continue-at",
            "-",
            "--silent",
            "--show-error",
            "--output",
        ])
        .arg(&partial)
        .arg(model.download_url()?)
        .stderr(Stdio::piped())
        .spawn()
        .wrap_err("could not start model download")?;
    let status = loop {
        downloaded_bytes.store(
            fs::metadata(&partial).map_or(0, |metadata| metadata.len()),
            Ordering::Relaxed,
        );
        if canceled.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("model download canceled");
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        thread::sleep(Duration::from_millis(100));
    };
    if !status.success() {
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            pipe.read_to_string(&mut stderr)?;
        }
        bail!(
            "Could not download the model from huggingface.co or its download CDN. \
             Check your connection, then retry in Settings. On a managed network, \
             ask IT to allow Hugging Face model downloads. \
             The app download uses a separate host. Download progress is kept for retry. \
             Details ({status}): {}",
            stderr.trim()
        );
    }
    check_canceled(canceled)?;
    ModelPreparationStage::Verifying.store(stage);
    if let Err(error) = verify_file_with_cancel(&partial, model, canceled) {
        if !canceled.load(Ordering::Relaxed) {
            let _ = fs::remove_file(&partial);
        }
        return Err(error);
    }
    File::open(&partial)?.sync_all()?;
    fs::rename(&partial, &destination)?;
    File::open(&directory)?.sync_all()?;
    write_verification_receipt(&destination, model)?;
    downloaded_bytes.store(artifact.bytes, Ordering::Relaxed);
    Ok(destination)
}

fn verification_receipt_path(model_path: &Path) -> PathBuf {
    model_path.with_extension("gguf.verified.json")
}

fn modified_ns(metadata: &fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos()
        .try_into()
        .ok()
}

fn write_verification_receipt(path: &Path, model: &ModelDefinition) -> Result<()> {
    let ModelRuntime::Gguf(artifact) = model.runtime else {
        bail!("{} has no artifact verification receipt", model.name);
    };
    let metadata = fs::metadata(path)?;
    let receipt = VerificationReceipt {
        bytes: artifact.bytes,
        sha256: artifact.sha256.into(),
        modified_ns: modified_ns(&metadata)
            .ok_or_else(|| color_eyre::eyre::eyre!("model modification time is unavailable"))?,
    };
    let destination = verification_receipt_path(path);
    let temporary = destination.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec(&receipt)?)?;
    File::open(&temporary)?.sync_all()?;
    fs::rename(temporary, destination)?;
    File::open(
        path.parent()
            .ok_or_else(|| color_eyre::eyre::eyre!("model path has no parent"))?,
    )?
    .sync_all()?;
    Ok(())
}

fn check_canceled(canceled: &AtomicBool) -> Result<()> {
    if canceled.load(Ordering::Relaxed) {
        bail!("model download canceled");
    }
    Ok(())
}

#[cfg(test)]
fn verify_file(path: &Path, model: &ModelDefinition) -> Result<()> {
    verify_file_with_cancel(path, model, &AtomicBool::new(false))
}

fn verify_file_with_cancel(
    path: &Path,
    model: &ModelDefinition,
    canceled: &AtomicBool,
) -> Result<()> {
    let ModelRuntime::Gguf(artifact) = model.runtime else {
        bail!("{} has no artifact to verify", model.name);
    };
    let metadata = fs::metadata(path)?;
    if metadata.len() != artifact.bytes {
        bail!(
            "downloaded {} bytes for {}, expected {}",
            metadata.len(),
            model.name,
            artifact.bytes
        );
    }
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        check_canceled(canceled)?;
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let actual = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if actual != artifact.sha256 {
        bail!("checksum mismatch for {}", model.name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn artifact_verification_checks_size_and_checksum() {
        const FIXTURE_ARTIFACT: GgufArtifact = GgufArtifact {
            filename: "fixture.gguf",
            revision: "fixture",
            repository: "fixture",
            bytes: 5,
            sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            architecture: "fixture",
            variant: "fixture",
        };
        let model = ModelDefinition {
            id: TranscriptionModelId::ParakeetV3,
            name: "fixture",
            realtime: "fixture",
            realtime_context: "fixture",
            quality: "fixture",
            quality_context: "fixture",
            coverage: "fixture",
            timestamps: "fixture",
            runtime: ModelRuntime::Gguf(&FIXTURE_ARTIFACT),
            languages: &["en"],
            accepts_language_hint: false,
            supports_language_detection: false,
            supports_recognition_hints: false,
        };
        let path = std::env::temp_dir().join(format!(
            "hex-model-verification-{}-{}",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, b"hello").unwrap();
        assert!(verify_file(&path, &model).is_ok());
        fs::write(&path, b"world").unwrap();
        assert!(verify_file(&path, &model).is_err());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn verification_receipt_is_invalidated_when_the_artifact_changes() {
        const FIXTURE_ARTIFACT: GgufArtifact = GgufArtifact {
            filename: "fixture.gguf",
            revision: "fixture",
            repository: "fixture",
            bytes: 5,
            sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            architecture: "fixture",
            variant: "fixture",
        };
        let model = ModelDefinition {
            id: TranscriptionModelId::ParakeetV3,
            name: "fixture",
            realtime: "fixture",
            realtime_context: "fixture",
            quality: "fixture",
            quality_context: "fixture",
            coverage: "fixture",
            timestamps: "fixture",
            runtime: ModelRuntime::Gguf(&FIXTURE_ARTIFACT),
            languages: &["en"],
            accepts_language_hint: false,
            supports_language_detection: false,
            supports_recognition_hints: false,
        };
        let path = std::env::temp_dir().join(format!(
            "hex-model-receipt-{}-{}",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, b"hello").unwrap();
        write_verification_receipt(&path, &model).unwrap();
        assert!(verification_receipt_matches(&path, &model));
        thread::sleep(Duration::from_millis(2));
        fs::write(&path, b"world").unwrap();
        assert!(!verification_receipt_matches(&path, &model));
        let _ = fs::remove_file(verification_receipt_path(&path));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn canceled_downloads_stop_before_io() {
        let canceled = AtomicBool::new(true);
        assert!(check_canceled(&canceled).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn installed_only_verification_never_downloads_or_replaces_an_invalid_model() {
        const ARTIFACT: GgufArtifact = GgufArtifact {
            filename: "fixture.gguf",
            revision: "fixture",
            repository: "fixture",
            bytes: 5,
            sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            architecture: "fixture",
            variant: "fixture",
        };
        let model = ModelDefinition {
            runtime: ModelRuntime::Gguf(&ARTIFACT),
            ..MODELS[0]
        };
        let directory = std::env::temp_dir().join(format!(
            "hex-installed-model-{}-{}",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("fixture.gguf");
        let canceled = AtomicBool::new(false);
        assert!(verify_installed_at(&path, &model, &canceled).is_err());
        assert!(!path.exists());
        fs::write(&path, b"hello").unwrap();
        verify_installed_at(&path, &model, &canceled).unwrap();
        assert!(verification_receipt_matches(&path, &model));
        thread::sleep(Duration::from_millis(2));
        fs::write(&path, b"world").unwrap();
        assert!(verify_installed_at(&path, &model, &canceled).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"world");
        assert!(!path.with_extension("gguf.partial").exists());
        canceled.store(true, Ordering::Relaxed);
        assert!(verify_installed_at(&path, &model, &canceled).is_err());
        fs::remove_file(verification_receipt_path(&path)).unwrap();
        fs::remove_file(path).unwrap();
        fs::remove_file(directory.join(".download.lock")).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}
