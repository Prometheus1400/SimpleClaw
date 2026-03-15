#[cfg(feature = "audio")]
pub(crate) mod cli;
pub(crate) mod config;

#[cfg(feature = "audio")]
mod decode;
#[cfg(feature = "audio")]
mod transcribe;
#[cfg(feature = "audio")]
mod tts;
#[cfg(feature = "audio")]
mod voice;

use async_trait::async_trait;
use std::path::Path;

use crate::channels::OutboundVoiceMessage;
pub(crate) use config::{AudioConfig, TtsMode};
#[cfg(feature = "audio")]
pub(crate) use transcribe::WhisperTranscriber;
#[cfg(feature = "audio")]
pub(crate) use tts::PiperSynthesizer;

use crate::error::FrameworkError;

#[async_trait]
pub(crate) trait Transcriber: Send + Sync {
    async fn transcribe(
        &self,
        audio_bytes: &[u8],
        filename: &str,
    ) -> Result<String, FrameworkError>;
}

#[async_trait]
pub(crate) trait Synthesizer: Send + Sync {
    async fn synthesize(&self, text: &str) -> Result<Vec<u8>, FrameworkError>;

    fn output_filename(&self) -> &str;
}

#[cfg(feature = "audio")]
pub(crate) async fn prepare_discord_voice_message(
    ffmpeg_binary: &Path,
    wav_bytes: &[u8],
) -> Result<OutboundVoiceMessage, FrameworkError> {
    voice::prepare_discord_voice_message(ffmpeg_binary, wav_bytes).await
}

#[cfg(not(feature = "audio"))]
pub(crate) async fn prepare_discord_voice_message(
    _ffmpeg_binary: &Path,
    _wav_bytes: &[u8],
) -> Result<OutboundVoiceMessage, FrameworkError> {
    Err(FrameworkError::Tool(
        "audio support is not enabled in this build".to_owned(),
    ))
}
