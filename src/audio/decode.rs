use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::error::FrameworkError;

pub(crate) async fn decode_to_pcm_f32(
    ffmpeg_binary: &std::path::Path,
    audio_bytes: &[u8],
    filename: &str,
) -> Result<Vec<f32>, FrameworkError> {
    let binary = ffmpeg_binary
        .to_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            FrameworkError::Config(
                "audio.transcription.ffmpeg_binary must be configured".to_owned(),
            )
        })?;

    let mut child = Command::new(binary)
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            "pipe:0",
            "-vn",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-f",
            "f32le",
            "pipe:1",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| {
            FrameworkError::Tool(format!("failed to start ffmpeg for '{filename}': {err}"))
        })?;

    let Some(mut stdin) = child.stdin.take() else {
        return Err(FrameworkError::Tool(
            "ffmpeg stdin was not available".to_owned(),
        ));
    };
    stdin.write_all(audio_bytes).await.map_err(|err| {
        FrameworkError::Tool(format!("failed to stream audio bytes to ffmpeg: {err}"))
    })?;
    drop(stdin);

    let output = child.wait_with_output().await.map_err(|err| {
        FrameworkError::Tool(format!("ffmpeg failed while decoding '{filename}': {err}"))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(FrameworkError::Tool(format!(
            "ffmpeg could not decode '{filename}': {}",
            stderr.trim()
        )));
    }
    if output.stdout.len() % std::mem::size_of::<f32>() != 0 {
        return Err(FrameworkError::Tool(format!(
            "ffmpeg returned truncated PCM output for '{filename}'"
        )));
    }

    let samples = output
        .stdout
        .chunks_exact(std::mem::size_of::<f32>())
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect::<Vec<_>>();
    if samples.is_empty() {
        return Err(FrameworkError::Tool(format!(
            "ffmpeg produced no audio samples for '{filename}'"
        )));
    }
    Ok(samples)
}
