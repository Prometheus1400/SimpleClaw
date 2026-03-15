use std::env;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use color_eyre::eyre::WrapErr;
use flate2::read::GzDecoder;
use tar::Archive;
use tokio::process::Command;

use crate::cli::{AudioAction, AudioInstallTarget, Cli};
use crate::config::LoadedConfig;
use crate::paths::AppPaths;

const WHISPER_MODEL_NAME: &str = "base.en";
const WHISPER_MODEL_FILE: &str = "ggml-base.en.bin";
const WHISPER_MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin";

const PIPER_VOICE_NAME: &str = "en_US-lessac-medium";
const PIPER_VOICE_MODEL_FILE: &str = "en_US-lessac-medium.onnx";
const PIPER_VOICE_CONFIG_FILE: &str = "en_US-lessac-medium.onnx.json";
const PIPER_VOICE_MODEL_URL: &str = "https://huggingface.co/rhasspy/piper-voices/resolve/v1.0.0/en/en_US/lessac/medium/en_US-lessac-medium.onnx";
const PIPER_VOICE_CONFIG_URL: &str = "https://huggingface.co/rhasspy/piper-voices/resolve/v1.0.0/en/en_US/lessac/medium/en_US-lessac-medium.onnx.json";

const PIPER_LINUX_AMD64_URL: &str =
    "https://github.com/rhasspy/piper/releases/download/v1.2.0/piper_amd64.tar.gz";
const PIPER_LINUX_ARM64_URL: &str =
    "https://github.com/rhasspy/piper/releases/download/v1.2.0/piper_arm64.tar.gz";

pub(crate) async fn handle_action(action: &AudioAction, cli: &Cli) -> color_eyre::Result<()> {
    match action {
        AudioAction::Status => show_status(cli).await,
        AudioAction::List => {
            list_assets();
            Ok(())
        }
        AudioAction::Install { target } => match target {
            AudioInstallTarget::Whisper { force } => install_whisper(*force).await,
            AudioInstallTarget::Piper { force } => install_piper(*force).await,
        },
    }
}

async fn show_status(cli: &Cli) -> color_eyre::Result<()> {
    let paths = AppPaths::resolve().wrap_err("failed to resolve ~/.simpleclaw paths")?;
    let status = AudioAssetStatus::discover(&paths);

    println!("Audio assets");
    println!(
        "Whisper model ({WHISPER_MODEL_NAME}): {}",
        render_presence(status.whisper_model_present, &status.whisper_model_path)
    );
    println!(
        "ffmpeg: {}",
        status
            .ffmpeg_binary
            .as_ref()
            .map(|path| format!("found at {}", display_path(path)))
            .unwrap_or_else(|| "not found on PATH".to_owned())
    );
    println!(
        "Piper binary: {}",
        status
            .piper_binary
            .as_ref()
            .map(|path| format!("found at {}", display_path(path)))
            .unwrap_or_else(|| "not installed".to_owned())
    );
    println!(
        "Piper voice ({PIPER_VOICE_NAME}): {}",
        if status.piper_voice_ready {
            format!(
                "installed at {}",
                display_path(&status.piper_voice_model_path)
            )
        } else {
            format!(
                "missing at {}",
                display_path(&status.piper_voice_model_path)
            )
        }
    );
    println!(
        "Piper voice metadata: {}",
        render_presence(
            status.piper_voice_config_path.exists(),
            &status.piper_voice_config_path
        )
    );

    if let Ok(loaded) = LoadedConfig::load(cli.workspace.as_deref()) {
        let observations = config_observations(&loaded, &status);
        if !observations.is_empty() {
            println!();
            println!("Config observations");
            for observation in observations {
                println!("- {observation}");
            }
        }
    }

    println!();
    println!("Suggested config");
    println!("{}", render_config_snippet(&status));
    if !status.whisper_model_present || status.piper_binary.is_none() || !status.piper_voice_ready {
        println!("Next steps");
        if !status.whisper_model_present {
            println!("- simpleclaw audio install whisper");
        }
        if status.piper_binary.is_none() || !status.piper_voice_ready {
            println!("- simpleclaw audio install piper");
        }
    }

    Ok(())
}

fn list_assets() {
    println!("Supported audio assets");
    println!("- whisper model: {WHISPER_MODEL_NAME} ({WHISPER_MODEL_FILE})");
    println!("- piper voice: {PIPER_VOICE_NAME} ({PIPER_VOICE_MODEL_FILE})");
    println!("- piper executable:");
    println!("  macOS: managed virtualenv (~/.simpleclaw/venvs/piper)");
    println!("  Linux x86_64/aarch64: official release archive");
}

async fn install_whisper(force: bool) -> color_eyre::Result<()> {
    let paths = AppPaths::resolve().wrap_err("failed to resolve ~/.simpleclaw paths")?;
    paths.ensure_base_dir()?;
    paths.ensure_audio_dirs()?;

    let dest = whisper_model_path(&paths);
    let downloaded = install_file(WHISPER_MODEL_URL, &dest, force)
        .await
        .wrap_err("failed to install whisper model")?;

    println!(
        "{} Whisper model at {}",
        if downloaded { "Installed" } else { "Reused" },
        display_path(&dest)
    );
    println!();
    println!("Suggested config");
    println!(
        "{}",
        render_config_snippet(&AudioAssetStatus {
            whisper_model_present: true,
            ffmpeg_binary: find_executable("ffmpeg"),
            piper_binary: discover_piper_binary(&paths),
            piper_voice_ready: piper_voice_model_path(&paths).exists()
                && piper_voice_config_path(&paths).exists(),
            whisper_model_path: dest,
            piper_voice_model_path: piper_voice_model_path(&paths),
            piper_voice_config_path: piper_voice_config_path(&paths),
        })
    );
    Ok(())
}

async fn install_piper(force: bool) -> color_eyre::Result<()> {
    let paths = AppPaths::resolve().wrap_err("failed to resolve ~/.simpleclaw paths")?;
    paths.ensure_base_dir()?;
    paths.ensure_audio_dirs()?;

    let binary_path = ensure_piper_binary(&paths, force)
        .await
        .wrap_err("failed to install or locate piper executable")?;
    let voice_model_path = piper_voice_model_path(&paths);
    let voice_config_path = piper_voice_config_path(&paths);
    let model_downloaded = install_file(PIPER_VOICE_MODEL_URL, &voice_model_path, force)
        .await
        .wrap_err("failed to install piper voice model")?;
    let config_downloaded = install_file(PIPER_VOICE_CONFIG_URL, &voice_config_path, force)
        .await
        .wrap_err("failed to install piper voice metadata")?;

    println!(
        "{} Piper executable at {}",
        if force
            || !find_executable("piper")
                .as_ref()
                .is_some_and(|existing| existing == &binary_path)
        {
            "Installed"
        } else {
            "Using"
        },
        display_path(&binary_path)
    );
    println!(
        "{} Piper voice model at {}",
        if model_downloaded || config_downloaded {
            "Installed"
        } else {
            "Reused"
        },
        display_path(&voice_model_path)
    );
    println!();
    println!("Suggested config");
    println!(
        "{}",
        render_config_snippet(&AudioAssetStatus {
            whisper_model_present: whisper_model_path(&paths).exists(),
            ffmpeg_binary: find_executable("ffmpeg"),
            piper_binary: Some(binary_path),
            piper_voice_ready: true,
            whisper_model_path: whisper_model_path(&paths),
            piper_voice_model_path: voice_model_path,
            piper_voice_config_path: voice_config_path,
        })
    );
    Ok(())
}

async fn ensure_piper_binary(paths: &AppPaths, force: bool) -> color_eyre::Result<PathBuf> {
    if cfg!(target_os = "macos") {
        return ensure_piper_binary_macos(force).await;
    }
    if cfg!(target_os = "linux") {
        return ensure_piper_binary_linux(paths, force).await;
    }

    Err(color_eyre::eyre::eyre!(
        "piper installation is only supported on macOS and Linux in this release"
    ))
}

async fn ensure_piper_binary_macos(force: bool) -> color_eyre::Result<PathBuf> {
    let paths = AppPaths::resolve().wrap_err("failed to resolve ~/.simpleclaw paths")?;
    let venv_dir = piper_venv_dir(&paths);
    let python_in_venv = piper_venv_python_path(&paths);
    let piper_in_venv = piper_venv_binary_path(&paths);

    if force && venv_dir.exists() {
        fs::remove_dir_all(&venv_dir)
            .wrap_err_with(|| format!("failed to clear {}", venv_dir.display()))?;
    }

    if !force && piper_in_venv.is_file() && managed_piper_env_is_healthy(&python_in_venv).await? {
        return Ok(piper_in_venv);
    }

    fs::create_dir_all(&paths.venvs_dir)
        .wrap_err_with(|| format!("failed to create {}", paths.venvs_dir.display()))?;

    let python3 = find_executable("python3")
        .ok_or_else(|| color_eyre::eyre::eyre!("python3 is required to install Piper on macOS"))?;
    let venv_status = Command::new(&python3)
        .args(["-m", "venv"])
        .arg(&venv_dir)
        .status()
        .await
        .wrap_err("failed to create the Piper virtual environment")?;
    if !venv_status.success() {
        return Err(color_eyre::eyre::eyre!(
            "python3 -m venv failed while creating the Piper environment"
        ));
    }

    let pip_status = Command::new(&python_in_venv)
        .args([
            "-m",
            "pip",
            "install",
            "--upgrade",
            "pip",
            "piper-tts",
            "pathvalidate",
        ])
        .status()
        .await
        .wrap_err("failed to install piper-tts into the managed virtual environment")?;
    if !pip_status.success() {
        return Err(color_eyre::eyre::eyre!(
            "pip failed to install piper-tts into the managed virtual environment"
        ));
    }

    if !piper_in_venv.is_file() {
        return Err(color_eyre::eyre::eyre!(
            "piper-tts installed successfully, but no Piper executable was found at {}",
            piper_in_venv.display()
        ));
    }
    if !managed_piper_env_is_healthy(&python_in_venv).await? {
        return Err(color_eyre::eyre::eyre!(
            "piper-tts installed, but the managed Piper environment is still missing runtime dependencies"
        ));
    }

    Ok(piper_in_venv)
}

async fn managed_piper_env_is_healthy(python_in_venv: &Path) -> color_eyre::Result<bool> {
    if !python_in_venv.is_file() {
        return Ok(false);
    }

    let status = Command::new(python_in_venv)
        .args(["-c", "import piper.__main__; import pathvalidate"])
        .status()
        .await
        .wrap_err("failed to validate the managed Piper environment")?;
    Ok(status.success())
}

async fn ensure_piper_binary_linux(paths: &AppPaths, force: bool) -> color_eyre::Result<PathBuf> {
    let install_root = paths.bin_dir.join("piper");
    let binary_path = install_root.join("piper");
    if binary_path.exists() && !force {
        return Ok(binary_path);
    }

    if install_root.exists() {
        fs::remove_dir_all(&install_root)
            .wrap_err_with(|| format!("failed to clear {}", install_root.display()))?;
    }
    fs::create_dir_all(&paths.bin_dir)
        .wrap_err_with(|| format!("failed to create {}", paths.bin_dir.display()))?;

    let url = match env::consts::ARCH {
        "x86_64" => PIPER_LINUX_AMD64_URL,
        "aarch64" => PIPER_LINUX_ARM64_URL,
        arch => {
            return Err(color_eyre::eyre::eyre!(
                "unsupported Linux architecture for Piper installation: {arch}"
            ));
        }
    };
    let archive_bytes = download_bytes(url)
        .await
        .wrap_err("failed to download Piper release archive")?;
    let mut archive = Archive::new(GzDecoder::new(Cursor::new(archive_bytes)));
    archive.unpack(&paths.bin_dir).wrap_err_with(|| {
        format!(
            "failed to unpack Piper archive into {}",
            paths.bin_dir.display()
        )
    })?;
    if !binary_path.exists() {
        return Err(color_eyre::eyre::eyre!(
            "Piper archive extracted but binary was not found at {}",
            binary_path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(&binary_path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&binary_path, permissions)?;
    }
    Ok(binary_path)
}

async fn install_file(url: &str, dest: &Path, force: bool) -> color_eyre::Result<bool> {
    if dest.exists() && !force && file_is_nonempty(dest)? {
        return Ok(false);
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
    }
    let bytes = download_bytes(url).await?;
    if bytes.is_empty() {
        return Err(color_eyre::eyre::eyre!("downloaded empty file from {url}"));
    }
    fs::write(dest, &bytes).wrap_err_with(|| format!("failed to write {}", dest.display()))?;
    if !file_is_nonempty(dest)? {
        return Err(color_eyre::eyre::eyre!(
            "installed file is empty: {}",
            dest.display()
        ));
    }
    Ok(true)
}

async fn download_bytes(url: &str) -> color_eyre::Result<Vec<u8>> {
    let client = reqwest::Client::new();
    let response = client
        .get(url)
        .send()
        .await
        .wrap_err_with(|| format!("request failed for {url}"))?
        .error_for_status()
        .wrap_err_with(|| format!("download failed for {url}"))?;
    let bytes = response
        .bytes()
        .await
        .wrap_err_with(|| format!("failed reading response body for {url}"))?;
    Ok(bytes.to_vec())
}

fn file_is_nonempty(path: &Path) -> color_eyre::Result<bool> {
    Ok(fs::metadata(path)
        .wrap_err_with(|| format!("failed to stat {}", path.display()))?
        .len()
        > 0)
}

struct AudioAssetStatus {
    whisper_model_present: bool,
    ffmpeg_binary: Option<PathBuf>,
    piper_binary: Option<PathBuf>,
    piper_voice_ready: bool,
    whisper_model_path: PathBuf,
    piper_voice_model_path: PathBuf,
    piper_voice_config_path: PathBuf,
}

impl AudioAssetStatus {
    fn discover(paths: &AppPaths) -> Self {
        Self {
            whisper_model_present: whisper_model_path(paths).exists(),
            ffmpeg_binary: find_executable("ffmpeg"),
            piper_binary: discover_piper_binary(paths),
            piper_voice_ready: piper_voice_model_path(paths).exists()
                && piper_voice_config_path(paths).exists(),
            whisper_model_path: whisper_model_path(paths),
            piper_voice_model_path: piper_voice_model_path(paths),
            piper_voice_config_path: piper_voice_config_path(paths),
        }
    }
}

fn config_observations(loaded: &LoadedConfig, status: &AudioAssetStatus) -> Vec<String> {
    let mut observations = Vec::new();
    if loaded.global.audio.transcription.enabled && !status.whisper_model_present {
        observations.push(format!(
            "transcription is enabled, but the configured model is not installed at {}",
            display_path(&loaded.global.audio.transcription.model_path)
        ));
    }
    if loaded.global.audio.transcription.model_path != status.whisper_model_path {
        observations.push(format!(
            "configured transcription model path differs from managed default: {}",
            display_path(&loaded.global.audio.transcription.model_path)
        ));
    }
    if loaded.global.audio.tts.mode.is_enabled() {
        if status.piper_binary.is_none() {
            observations.push("tts is enabled, but no piper executable was found".to_owned());
        }
        if !status.piper_voice_ready {
            observations
                .push("tts is enabled, but the managed Piper voice assets are missing".to_owned());
        }
    }
    if loaded.global.audio.tts.piper_model != status.piper_voice_model_path
        && !loaded.global.audio.tts.piper_model.as_os_str().is_empty()
    {
        observations.push(format!(
            "configured Piper model path differs from managed default: {}",
            display_path(&loaded.global.audio.tts.piper_model)
        ));
    }
    if let Some(binary) = status.piper_binary.as_ref()
        && loaded.global.audio.tts.piper_binary != *binary
        && !loaded.global.audio.tts.piper_binary.as_os_str().is_empty()
    {
        observations.push(format!(
            "configured Piper binary path differs from detected install: {}",
            display_path(&loaded.global.audio.tts.piper_binary)
        ));
    }
    observations
}

fn render_config_snippet(status: &AudioAssetStatus) -> String {
    let mut output = String::from("audio:\n");
    output.push_str("  transcription:\n");
    output.push_str(&format!(
        "    enabled: {}\n",
        if status.whisper_model_present {
            "true"
        } else {
            "false"
        }
    ));
    output.push_str(&format!(
        "    model_path: {}\n",
        display_path(&status.whisper_model_path)
    ));
    output.push_str("    language: en\n");
    output.push_str(&format!(
        "    ffmpeg_binary: {}\n",
        status
            .ffmpeg_binary
            .as_ref()
            .map(|_| "ffmpeg".to_owned())
            .unwrap_or_else(|| "ffmpeg".to_owned())
    ));
    output.push_str("  tts:\n");
    output.push_str(&format!(
        "    mode: {}\n",
        if status.piper_binary.is_some() && status.piper_voice_ready {
            "auto"
        } else {
            "off"
        }
    ));
    if let Some(binary) = status.piper_binary.as_ref() {
        output.push_str(&format!("    piper_binary: {}\n", display_path(binary)));
    }
    if status.piper_voice_ready {
        output.push_str(&format!(
            "    piper_model: {}\n",
            display_path(&status.piper_voice_model_path)
        ));
    }
    output
}

fn whisper_model_path(paths: &AppPaths) -> PathBuf {
    paths.models_dir.join(WHISPER_MODEL_FILE)
}

fn piper_voice_model_path(paths: &AppPaths) -> PathBuf {
    paths.models_dir.join("piper").join(PIPER_VOICE_MODEL_FILE)
}

fn piper_voice_config_path(paths: &AppPaths) -> PathBuf {
    paths.models_dir.join("piper").join(PIPER_VOICE_CONFIG_FILE)
}

fn piper_venv_dir(paths: &AppPaths) -> PathBuf {
    paths.venvs_dir.join("piper")
}

fn piper_venv_python_path(paths: &AppPaths) -> PathBuf {
    piper_venv_dir(paths).join("bin").join("python")
}

fn piper_venv_binary_path(paths: &AppPaths) -> PathBuf {
    piper_venv_dir(paths).join("bin").join("piper")
}

fn discover_piper_binary(paths: &AppPaths) -> Option<PathBuf> {
    let managed_venv = piper_venv_binary_path(paths);
    if managed_venv.exists() {
        return Some(managed_venv);
    }
    let managed = paths.bin_dir.join("piper").join("piper");
    if managed.exists() {
        return Some(managed);
    }
    find_executable("piper")
}

fn render_presence(present: bool, path: &Path) -> String {
    if present {
        format!("installed at {}", display_path(path))
    } else {
        format!("missing at {}", display_path(path))
    }
}

fn display_path(path: &Path) -> String {
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return path.display().to_string();
    };
    if path == home {
        return "~".to_owned();
    }
    if let Ok(remainder) = path.strip_prefix(&home) {
        if remainder.as_os_str().is_empty() {
            "~".to_owned()
        } else {
            format!("~/{}", remainder.display())
        }
    } else {
        path.display().to_string()
    }
}

fn find_executable(name: &str) -> Option<PathBuf> {
    if name.contains(std::path::MAIN_SEPARATOR) {
        let path = PathBuf::from(name);
        return path.is_file().then_some(path);
    }

    let path_var = env::var_os("PATH")?;
    env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        AudioAssetStatus, display_path, piper_venv_binary_path, piper_venv_dir,
        render_config_snippet, whisper_model_path,
    };
    use crate::paths::AppPaths;

    fn test_paths() -> AppPaths {
        let base_dir = PathBuf::from("/tmp/simpleclaw-audio-test");
        AppPaths {
            base_dir: base_dir.clone(),
            bin_dir: base_dir.join("bin"),
            models_dir: base_dir.join("models"),
            venvs_dir: base_dir.join("venvs"),
            config_path: base_dir.join("config.yaml"),
            secrets_path: base_dir.join("secrets.yaml"),
            db_path: base_dir.join("db/short.db"),
            long_term_db_path: base_dir.join("db/long.db"),
            cron_db_path: base_dir.join("db/cron.db"),
            fastembed_cache_dir: base_dir.join(".fastembed"),
            logs_dir: base_dir.join("logs"),
            log_path: base_dir.join("logs/service.log"),
            run_dir: base_dir.join("run"),
            pid_path: base_dir.join("run/service.pid"),
        }
    }

    #[test]
    fn display_path_collapses_home_prefix() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        assert_eq!(
            display_path(&home.join(".simpleclaw/models/a.bin")),
            "~/.simpleclaw/models/a.bin"
        );
    }

    #[test]
    fn render_config_snippet_enables_sections_when_assets_are_ready() {
        let paths = test_paths();
        let status = AudioAssetStatus {
            whisper_model_present: true,
            ffmpeg_binary: Some(PathBuf::from("/opt/homebrew/bin/ffmpeg")),
            piper_binary: Some(piper_venv_binary_path(&paths)),
            piper_voice_ready: true,
            whisper_model_path: whisper_model_path(&paths),
            piper_voice_model_path: paths.models_dir.join("piper/en_US-lessac-medium.onnx"),
            piper_voice_config_path: paths.models_dir.join("piper/en_US-lessac-medium.onnx.json"),
        };

        let snippet = render_config_snippet(&status);
        assert!(snippet.contains("transcription:\n    enabled: true"));
        assert!(snippet.contains("tts:\n    mode: auto"));
        assert!(snippet.contains("piper_binary: /tmp/simpleclaw-audio-test/venvs/piper/bin/piper"));
    }

    #[test]
    fn managed_piper_venv_paths_are_stable() {
        let paths = test_paths();
        assert_eq!(
            piper_venv_dir(&paths),
            PathBuf::from("/tmp/simpleclaw-audio-test/venvs/piper")
        );
        assert_eq!(
            piper_venv_binary_path(&paths),
            PathBuf::from("/tmp/simpleclaw-audio-test/venvs/piper/bin/piper")
        );
    }
}
