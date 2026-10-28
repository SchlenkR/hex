use color_eyre::eyre::Result;
use serde::Deserialize;

use crate::apple_speech::AppleSpeech;
use crate::parakeet::Parakeet;
use crate::transcription_models::{ModelRuntime, TranscriptionSelection, validate};

pub enum Transcriber {
    Gguf(Box<Parakeet>),
    AppleSpeech(AppleSpeech),
}

#[derive(Default)]
pub struct WarmTranscriber {
    active: Option<Transcriber>,
}

#[derive(Debug, Deserialize)]
pub struct Transcript {
    pub text: String,
}

impl Transcriber {
    pub fn load() -> Result<Self> {
        let (_, selection) = crate::app_settings::transcription_selection();
        Self::load_selection(&selection)
    }

    pub fn load_selection(selection: &TranscriptionSelection) -> Result<Self> {
        match validate(selection)?.runtime {
            ModelRuntime::Gguf(_) => Parakeet::load_selection(selection)
                .map(Box::new)
                .map(Self::Gguf),
            ModelRuntime::AppleSpeech => AppleSpeech::load(selection).map(Self::AppleSpeech),
        }
    }

    pub fn matches_selection(&self, selection: &TranscriptionSelection) -> bool {
        match self {
            Self::Gguf(model) => model.matches_selection(selection),
            Self::AppleSpeech(model) => model.matches_selection(selection),
        }
    }

    pub fn prepare_samples(&self, mut samples: Vec<f32>) -> Vec<f32> {
        if matches!(self, Self::Gguf(_)) {
            prepare_gguf_samples(&mut samples);
        }
        samples
    }

    pub fn transcribe(&mut self, samples: &[f32]) -> Result<String> {
        match self {
            Self::Gguf(model) => model.transcribe(samples),
            Self::AppleSpeech(model) => model.transcribe(samples).map(|result| result.text),
        }
    }
}

fn prepare_gguf_samples(samples: &mut Vec<f32>) {
    crate::dictation::pad_for_parakeet(samples);
}

impl WarmTranscriber {
    pub fn load() -> Result<Self> {
        Ok(Self {
            active: Some(Transcriber::load()?),
        })
    }

    pub fn activate(&mut self, selection: &TranscriptionSelection) -> Result<&mut Transcriber> {
        if !self
            .active
            .as_ref()
            .is_some_and(|model| model.matches_selection(selection))
        {
            let candidate = Transcriber::load_selection(selection)?;
            self.active = Some(candidate);
        }
        Ok(self
            .active
            .as_mut()
            .expect("activated transcriber must be available"))
    }

    pub fn release(&mut self) -> bool {
        self.active.take().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::Transcript;

    #[test]
    fn apple_bridge_transcript_uses_the_canonical_wire_shape() {
        let transcript: Transcript = serde_json::from_str(
            r#"{"text":"hello","segments":[{"startMs":10,"endMs":40,"text":"hello"}]}"#,
        )
        .unwrap();

        assert_eq!(transcript.text, "hello");
    }
}
