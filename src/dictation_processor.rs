use crate::context::{ContextSelector, ContextSnapshot};
use crate::text_replacements::ReplacementSet;

#[derive(Clone, Default)]
pub struct Profile {
    replacements: ReplacementSet,
}

impl Profile {
    pub(crate) fn replacements(mut self, replacements: ReplacementSet) -> Self {
        self.replacements = replacements;
        self
    }
}

#[derive(Clone)]
struct ContextualProfile {
    selector: ContextSelector,
    profile: Profile,
}

#[derive(Clone)]
pub struct Profiles {
    default: Profile,
    contextual: Vec<ContextualProfile>,
}

impl Profiles {
    pub fn new(default: Profile) -> Self {
        Self {
            default,
            contextual: Vec::new(),
        }
    }

    pub fn application(self, application: impl Into<String>, profile: Profile) -> Self {
        self.push(ContextSelector::application(application.into()), profile)
    }

    pub fn browser_host(self, host: impl Into<String>, profile: Profile) -> Self {
        self.push(ContextSelector::browser_host(host.into()), profile)
    }

    fn push(mut self, selector: ContextSelector, profile: Profile) -> Self {
        if self
            .contextual
            .iter()
            .any(|existing| existing.selector == selector)
        {
            tracing::warn!(?selector, "ignored duplicate dictation processor selector");
            return self;
        }
        self.contextual
            .push(ContextualProfile { selector, profile });
        self
    }

    fn select<'a>(&'a self, context: &ContextSnapshot) -> &'a Profile {
        let selected = self
            .contextual
            .iter()
            .find(|candidate| {
                candidate.selector.is_browser() && candidate.selector.matches(context)
            })
            .or_else(|| {
                self.contextual.iter().find(|candidate| {
                    !candidate.selector.is_browser() && candidate.selector.matches(context)
                })
            });
        selected.map_or(&self.default, |selected| &selected.profile)
    }

    pub fn process(&self, transcript: &str, context: &ContextSnapshot) -> String {
        self.select(context).replacements.replace(transcript)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    fn context(application: &str, url: Option<&str>) -> ContextSnapshot {
        ContextSnapshot {
            application: Some(application.into()),
            browser_url: url.map(|url| Url::parse(url).unwrap()),
            window_title: None,
            input_revision: None,
        }
    }

    fn named(name: &str) -> Profile {
        Profile::default().replacements(ReplacementSet::new(&[
            crate::app_settings::TextReplacement {
                matched_phrase: "mode".into(),
                output: name.into(),
            },
        ]))
    }

    fn profiles() -> Profiles {
        Profiles::new(named("default"))
            .application("Slack", named("slack"))
            .browser_host("x.com", named("x"))
    }

    #[test]
    fn browser_host_beats_application_and_application_beats_default() {
        let profiles = profiles();
        assert_eq!(
            profiles.process(
                "mode",
                &context("Brave Browser", Some("https://x.com/home"))
            ),
            "x"
        );
        assert_eq!(profiles.process("mode", &context("Slack", None)), "slack");
        assert_eq!(profiles.process("mode", &context("Zed", None)), "default");
    }

    #[test]
    fn mode_corrections_apply_to_the_transcript() {
        let profile = Profile::default().replacements(ReplacementSet::new(&[
            crate::app_settings::TextReplacement {
                matched_phrase: "open code".into(),
                output: "OpenCode".into(),
            },
        ]));
        let profiles = Profiles::new(profile);

        assert_eq!(
            profiles.process("Use open code.", &context("Zed", None)),
            "Use OpenCode."
        );
    }
}
