use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum, Serialize, Deserialize)]
pub enum VersionSize {
    Original,
    Medium,
    Thumb,
    Adjusted,
    Alternative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum)]
pub enum LivePhotoSize {
    Original,
    Medium,
    Thumb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Domain {
    Com,
    Cn,
}

impl Domain {
    pub fn as_str(&self) -> &str {
        match self {
            Domain::Com => "com",
            Domain::Cn => "cn",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LogLevel {
    Debug,
    Info,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum FileMatchPolicy {
    #[value(name = "name-size-dedup-with-suffix")]
    NameSizeDedupWithSuffix,
    #[value(name = "name-id7")]
    NameId7,
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum RawTreatmentPolicy {
    #[value(name = "as-is")]
    AsIs,
    #[value(name = "original")]
    AsOriginal,
    #[value(name = "alternative")]
    AsAlternative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LivePhotoMovFilenamePolicy {
    Suffix,
    Original,
}
