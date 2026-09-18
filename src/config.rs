use std::sync::{OnceLock, RwLock};

use crate::app_settings::DictationProcessingSettings;
use crate::dictation_processor::{Profile, Profiles};

pub const INPUT_DEVICES: &[&str] = &["Universal Audio Thunderbolt", "Studio Display Microphone"];
static DICTATION_PROFILES: OnceLock<RwLock<Profiles>> = OnceLock::new();

pub fn dictation_profiles() -> Profiles {
    DICTATION_PROFILES
        .get_or_init(|| {
            RwLock::new(build_dictation_profiles(
                &DictationProcessingSettings::default(),
            ))
        })
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

pub fn update_dictation_profiles(settings: &DictationProcessingSettings) {
    let profiles = build_dictation_profiles(settings);
    if let Some(runtime) = DICTATION_PROFILES.get() {
        *runtime.write().unwrap_or_else(|error| error.into_inner()) = profiles;
    } else {
        let _ = DICTATION_PROFILES.set(RwLock::new(profiles));
    }
}

fn build_dictation_profiles(settings: &DictationProcessingSettings) -> Profiles {
    let mut profiles = Profiles::new(mode_profile(&settings.default_mode));
    for mode in &settings.modes {
        let profile = mode_profile(mode);
        for application in &mode.applications {
            profiles = profiles.application(application.clone(), profile.clone());
        }
        for host in &mode.browser_hosts {
            profiles = profiles.browser_host(host.clone(), profile.clone());
        }
    }
    profiles
}

fn mode_profile(mode: &crate::app_settings::DictationMode) -> Profile {
    Profile::default().replacements(crate::text_replacements::ReplacementSet::new(
        &mode.replacements,
    ))
}
