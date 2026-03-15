use std::io::Cursor;
use std::path::Path;

use base64::Engine;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::channels::OutboundVoiceMessage;
use crate::error::FrameworkError;

const DISCORD_WAVEFORM_BUCKETS: usize = 256;
const VOICE_FILENAME: &str = "voice-message.ogg";

pub(crate) async fn prepare_discord_voice_message(
    ffmpeg_binary: &Path,
    wav_bytes: &[u8],
) -> Result<OutboundVoiceMessage, FrameworkError> {
    let (samples, duration_secs) = parse_wav_samples(wav_bytes)?;
    let waveform = encode_waveform(&samples);
    let audio_bytes = transcode_wav_to_ogg_opus(ffmpeg_binary, wav_bytes).await?;

    Ok(OutboundVoiceMessage {
        audio_bytes,
        attachment_filename: VOICE_FILENAME.to_owned(),
        duration_secs,
        waveform,
    })
}

fn parse_wav_samples(wav_bytes: &[u8]) -> Result<(Vec<f32>, f64), FrameworkError> {
    let mut reader = hound::WavReader::new(Cursor::new(wav_bytes))
        .map_err(|err| FrameworkError::Tool(format!("failed to parse synthesized wav: {err}")))?;
    let spec = reader.spec();
    let channels = usize::from(spec.channels.max(1));
    let raw_samples = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| {
                FrameworkError::Tool(format!("failed to read wav float samples: {err}"))
            })?,
        hound::SampleFormat::Int => {
            let bits = u32::from(spec.bits_per_sample).saturating_sub(1);
            let scale = (1_i64 << bits) as f32;
            reader
                .samples::<i32>()
                .map(|sample| {
                    sample.map(|value| value as f32 / scale).map_err(|err| {
                        FrameworkError::Tool(format!("failed to read wav int samples: {err}"))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
        }
    };
    if raw_samples.is_empty() {
        return Err(FrameworkError::Tool(
            "synthesized wav did not contain any audio samples".to_owned(),
        ));
    }

    let mono_samples = raw_samples
        .chunks(channels)
        .map(|frame| frame.iter().copied().sum::<f32>() / frame.len() as f32)
        .collect::<Vec<_>>();
    let duration_secs = mono_samples.len() as f64 / f64::from(spec.sample_rate.max(1));
    Ok((mono_samples, duration_secs))
}

fn encode_waveform(samples: &[f32]) -> String {
    let mut waveform = Vec::with_capacity(DISCORD_WAVEFORM_BUCKETS);
    for bucket in 0..DISCORD_WAVEFORM_BUCKETS {
        let start = bucket * samples.len() / DISCORD_WAVEFORM_BUCKETS;
        let end = ((bucket + 1) * samples.len() / DISCORD_WAVEFORM_BUCKETS).min(samples.len());
        let amplitude = if start >= end {
            0.0
        } else {
            samples[start..end]
                .iter()
                .map(|sample| sample.abs())
                .fold(0.0_f32, f32::max)
        };
        waveform.push((amplitude.clamp(0.0, 1.0) * 255.0).round() as u8);
    }
    base64::engine::general_purpose::STANDARD.encode(waveform)
}

async fn transcode_wav_to_ogg_opus(
    ffmpeg_binary: &Path,
    wav_bytes: &[u8],
) -> Result<Vec<u8>, FrameworkError> {
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
            "-c:a",
            "libopus",
            "-b:a",
            "64k",
            "-f",
            "ogg",
            "pipe:1",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| {
            FrameworkError::Tool(format!(
                "failed to start ffmpeg for discord voice encoding: {err}"
            ))
        })?;

    let Some(mut stdin) = child.stdin.take() else {
        return Err(FrameworkError::Tool(
            "ffmpeg stdin was not available for voice encoding".to_owned(),
        ));
    };
    stdin.write_all(wav_bytes).await.map_err(|err| {
        FrameworkError::Tool(format!("failed to stream synthesized wav to ffmpeg: {err}"))
    })?;
    drop(stdin);

    let output = child.wait_with_output().await.map_err(|err| {
        FrameworkError::Tool(format!(
            "ffmpeg failed while encoding discord voice audio: {err}"
        ))
    })?;
    if !output.status.success() {
        return Err(FrameworkError::Tool(format!(
            "ffmpeg could not encode discord voice audio: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    if output.stdout.is_empty() {
        return Err(FrameworkError::Tool(
            "ffmpeg produced no encoded discord voice audio".to_owned(),
        ));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use base64::Engine;

    use super::encode_waveform;

    #[test]
    fn waveform_encoding_has_expected_length() {
        let encoded = encode_waveform(&vec![0.5; 1024]);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("base64 should decode");
        assert_eq!(decoded.len(), 256);
    }
}
