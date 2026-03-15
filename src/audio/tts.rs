use std::fs;
use std::path::PathBuf;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use uuid::Uuid;

use crate::audio::Synthesizer;
use crate::error::FrameworkError;

const PIPER_OUTPUT_FILE: &str = "response.wav";

pub(crate) struct PiperSynthesizer {
    piper_binary: PathBuf,
    piper_model: PathBuf,
    output_filename: String,
}

impl PiperSynthesizer {
    pub(crate) fn new(piper_binary: PathBuf, piper_model: PathBuf) -> Result<Self, FrameworkError> {
        if piper_binary.as_os_str().is_empty() {
            return Err(FrameworkError::Config(
                "audio.tts.piper_binary must be configured when TTS is enabled".to_owned(),
            ));
        }
        if piper_model.as_os_str().is_empty() {
            return Err(FrameworkError::Config(
                "audio.tts.piper_model must be configured when TTS is enabled".to_owned(),
            ));
        }

        Ok(Self {
            piper_binary,
            piper_model,
            output_filename: PIPER_OUTPUT_FILE.to_owned(),
        })
    }
}

#[async_trait]
impl Synthesizer for PiperSynthesizer {
    async fn synthesize(&self, text: &str) -> Result<Vec<u8>, FrameworkError> {
        let binary = self
            .piper_binary
            .to_str()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                FrameworkError::Config("audio.tts.piper_binary must be valid UTF-8".to_owned())
            })?;
        let model = self
            .piper_model
            .to_str()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                FrameworkError::Config("audio.tts.piper_model must be valid UTF-8".to_owned())
            })?;

        let output_path =
            std::env::temp_dir().join(format!("simpleclaw-piper-{}.wav", Uuid::new_v4()));
        let mut child = Command::new(binary)
            .args(["--model", model, "--output_file"])
            .arg(&output_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|err| FrameworkError::Tool(format!("failed to start piper: {err}")))?;

        let Some(mut stdin) = child.stdin.take() else {
            return Err(FrameworkError::Tool(
                "piper stdin was not available".to_owned(),
            ));
        };
        stdin
            .write_all(text.as_bytes())
            .await
            .map_err(|err| FrameworkError::Tool(format!("failed to write text to piper: {err}")))?;
        drop(stdin);

        let output = child
            .wait_with_output()
            .await
            .map_err(|err| FrameworkError::Tool(format!("piper process failed: {err}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = fs::remove_file(&output_path);
            return Err(FrameworkError::Tool(format!(
                "piper synthesis failed: {}",
                stderr.trim()
            )));
        }

        let audio = fs::read(&output_path)
            .map_err(|err| FrameworkError::Tool(format!("failed to read piper output: {err}")))?;
        let _ = fs::remove_file(&output_path);
        Ok(audio)
    }

    fn output_filename(&self) -> &str {
        &self.output_filename
    }
}
