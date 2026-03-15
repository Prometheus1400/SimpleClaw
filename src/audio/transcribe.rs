use std::path::PathBuf;
use std::time::Instant;

use async_trait::async_trait;
use tokio::task;

use super::decode::decode_to_pcm_f32;
use crate::audio::Transcriber;
use crate::error::FrameworkError;

pub(crate) struct WhisperTranscriber {
    model_path: PathBuf,
    language: Option<String>,
    ffmpeg_binary: PathBuf,
}

impl WhisperTranscriber {
    pub(crate) fn new(
        model_path: PathBuf,
        language: Option<String>,
        ffmpeg_binary: PathBuf,
    ) -> Result<Self, FrameworkError> {
        if model_path.as_os_str().is_empty() {
            return Err(FrameworkError::Config(
                "audio.transcription.model_path must be configured when transcription is enabled"
                    .to_owned(),
            ));
        }
        if ffmpeg_binary.as_os_str().is_empty() {
            return Err(FrameworkError::Config(
                "audio.transcription.ffmpeg_binary must be configured when transcription is enabled"
                    .to_owned(),
            ));
        }

        let validation_path = model_path.clone();
        let validation = whisper_rs::WhisperContext::new_with_params(
            validation_path.to_str().ok_or_else(|| {
                FrameworkError::Config(
                    "audio.transcription.model_path must be valid UTF-8".to_owned(),
                )
            })?,
            whisper_rs::WhisperContextParameters::default(),
        )
        .map_err(|err| {
            FrameworkError::Config(format!(
                "failed to load whisper model '{}': {err}",
                model_path.display()
            ))
        })?;
        drop(validation);

        Ok(Self {
            model_path,
            language,
            ffmpeg_binary,
        })
    }
}

#[async_trait]
impl Transcriber for WhisperTranscriber {
    async fn transcribe(
        &self,
        audio_bytes: &[u8],
        filename: &str,
    ) -> Result<String, FrameworkError> {
        let started = Instant::now();
        let samples = decode_to_pcm_f32(&self.ffmpeg_binary, audio_bytes, filename).await?;
        let model_path = self.model_path.clone();
        let language = self.language.clone();
        let filename = filename.to_owned();
        let log_filename = filename.clone();
        let transcript = task::spawn_blocking(move || {
            let model_path = model_path.to_str().ok_or_else(|| {
                FrameworkError::Config(
                    "audio.transcription.model_path must be valid UTF-8".to_owned(),
                )
            })?;
            let ctx = whisper_rs::WhisperContext::new_with_params(
                model_path,
                whisper_rs::WhisperContextParameters::default(),
            )
            .map_err(|err| {
                FrameworkError::Tool(format!(
                    "failed to initialize whisper context for '{}': {err}",
                    filename
                ))
            })?;
            let mut state = ctx.create_state().map_err(|err| {
                FrameworkError::Tool(format!(
                    "failed to create whisper state for '{}': {err}",
                    filename
                ))
            })?;

            let mut params =
                whisper_rs::FullParams::new(whisper_rs::SamplingStrategy::Greedy { best_of: 1 });
            params.set_n_threads(2);
            params.set_translate(false);
            params.set_print_progress(false);
            params.set_print_special(false);
            params.set_print_realtime(false);
            if let Some(language) = language.as_deref() {
                params.set_language(Some(language));
            }
            state.full(params, &samples).map_err(|err| {
                FrameworkError::Tool(format!(
                    "whisper transcription failed for '{}': {err}",
                    filename
                ))
            })?;

            let segment_count = state.full_n_segments().map_err(|err| {
                FrameworkError::Tool(format!(
                    "failed to read whisper segments for '{}': {err}",
                    filename
                ))
            })?;
            let mut transcript = String::new();
            for index in 0..segment_count {
                let segment = state.full_get_segment_text(index).map_err(|err| {
                    FrameworkError::Tool(format!(
                        "failed to read whisper segment {index} for '{}': {err}",
                        filename
                    ))
                })?;
                let segment = segment.trim();
                if segment.is_empty() {
                    continue;
                }
                if !transcript.is_empty() {
                    transcript.push(' ');
                }
                transcript.push_str(segment);
            }
            Ok::<_, FrameworkError>(transcript)
        })
        .await
        .map_err(|err| FrameworkError::Tool(format!("transcription task failed: {err}")))??;

        tracing::info!(
            status = "completed",
            file_name = %log_filename,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "audio transcription completed"
        );

        if transcript.trim().is_empty() {
            return Err(FrameworkError::Tool(format!(
                "audio transcription for '{}' was empty",
                log_filename
            )));
        }
        Ok(transcript)
    }
}
