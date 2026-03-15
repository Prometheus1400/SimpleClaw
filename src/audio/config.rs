use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::channels::InboundMessageKind;

/// Audio feature configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    pub transcription: TranscriptionConfig,
    pub tts: TtsConfig,
}

/// Inbound transcription configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TranscriptionConfig {
    pub enabled: bool,
    pub model_path: PathBuf,
    pub language: Option<String>,
    pub ffmpeg_binary: PathBuf,
}

impl Default for TranscriptionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model_path: PathBuf::new(),
            language: None,
            ffmpeg_binary: PathBuf::from("ffmpeg"),
        }
    }
}

/// Outbound text-to-speech configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TtsConfig {
    pub mode: TtsMode,
    pub piper_binary: PathBuf,
    pub piper_model: PathBuf,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            mode: TtsMode::Off,
            piper_binary: PathBuf::new(),
            piper_model: PathBuf::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TtsMode {
    #[default]
    Off,
    On,
    Auto,
}

impl TtsMode {
    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    pub fn should_synthesize(self, inbound_kind: InboundMessageKind) -> bool {
        match self {
            Self::Off => false,
            Self::On => true,
            Self::Auto => inbound_kind == InboundMessageKind::Voice,
        }
    }
}
