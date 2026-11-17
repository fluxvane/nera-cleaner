//! Data macOS downloads for its own features (Siri voices and speech
//! models, Apple Intelligence models). It lives in `/System/Library/AssetsV2`,
//! belongs to the system and is downloaded again while the feature is on, so
//! it is never cleaned: it is only measured, with a shortcut to the setting
//! that makes macOS remove it. Read-only.

use nera_core::fs_util::{dir_size, Cancel, SizeOptions};
use std::path::Path;

const ASSETS_ROOT: &str = "/System/Library/AssetsV2";
/// System Settings > Apple Intelligence & Siri (named "Siri" on older macOS).
pub const SIRI_SETTINGS_URL: &str = "x-apple.systempreferences:com.apple.Siri-Settings.extension";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    Siri,
    AppleIntelligence,
}

impl Feature {
    pub fn name(self) -> &'static str {
        match self {
            Feature::Siri => "Siri",
            Feature::AppleIntelligence => "Apple Intelligence",
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            Feature::Siri => "Voices and on-device speech models",
            Feature::AppleIntelligence => "On-device language and image models",
        }
    }

    pub fn settings_url(self) -> &'static str {
        SIRI_SETTINGS_URL
    }
}

/// Space one system feature's downloaded data uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemAsset {
    pub feature: Feature,
    pub bytes: u64,
}

/// Which feature an asset folder belongs to. Only folders that are
/// unmistakably one feature's are claimed: dictation, spelling and the
/// Xcode code model stay out.
pub fn classify(folder: &str) -> Option<Feature> {
    let name = folder.strip_prefix("com_apple_MobileAsset_")?;
    if name.starts_with("UAF_Siri_") || name.starts_with("Trial_Siri_") {
        Some(Feature::Siri)
    } else if [
        "UAF_FM_GenerativeModels",
        "UAF_FM_Visual",
        "UAF_FM_Overrides",
    ]
    .contains(&name)
    {
        Some(Feature::AppleIntelligence)
    } else {
        None
    }
}

/// Features with downloaded data, largest first.
pub fn system_assets(cancel: &Cancel) -> Vec<SystemAsset> {
    system_assets_in(Path::new(ASSETS_ROOT), cancel)
}

fn system_assets_in(root: &Path, cancel: &Cancel) -> Vec<SystemAsset> {
    let mut totals = [(Feature::Siri, 0u64), (Feature::AppleIntelligence, 0u64)];
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        let Some(feature) = classify(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        let bytes = dir_size(&entry.path(), &SizeOptions::default(), cancel);
        if let Some(total) = totals.iter_mut().find(|(f, _)| *f == feature) {
            total.1 += bytes;
        }
    }
    let mut out: Vec<SystemAsset> = totals
        .into_iter()
        .filter(|(_, bytes)| *bytes > 0)
        .map(|(feature, bytes)| SystemAsset { feature, bytes })
        .collect();
    out.sort_by_key(|a| std::cmp::Reverse(a.bytes));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_unmistakable_folders_are_claimed() {
        let siri = Some(Feature::Siri);
        assert_eq!(
            classify("com_apple_MobileAsset_UAF_Siri_Understanding"),
            siri
        );
        assert_eq!(
            classify("com_apple_MobileAsset_Trial_Siri_SiriTextToSpeech"),
            siri
        );
        assert_eq!(
            classify("com_apple_MobileAsset_UAF_FM_GenerativeModels"),
            Some(Feature::AppleIntelligence)
        );
        for other in [
            "com_apple_MobileAsset_UAF_FM_CodeLM",
            "com_apple_MobileAsset_UAF_Speech_AutomaticSpeechRecognition",
            "com_apple_MobileAsset_iOSSimulatorRuntime",
            "com_apple_MobileAsset_UAF_LinguisticData",
            "UAF_Siri_Understanding",
        ] {
            assert_eq!(classify(other), None, "{other}");
        }
    }

    #[test]
    fn sums_each_feature_and_skips_the_rest() {
        let root = std::env::temp_dir().join(format!("nera-assets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (dir, len) in [
            ("com_apple_MobileAsset_UAF_Siri_Understanding", 300_000),
            ("com_apple_MobileAsset_Trial_Siri_SiriTextToSpeech", 100_000),
            ("com_apple_MobileAsset_UAF_FM_CodeLM", 900_000),
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join("model.bin"), vec![1u8; len]).unwrap();
        }
        let found = system_assets_in(&root, &Cancel::new());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].feature, Feature::Siri);
        assert!(
            (400_000..800_000).contains(&found[0].bytes),
            "{}",
            found[0].bytes
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
