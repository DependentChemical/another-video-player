use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::{Error, RESOLUTION_TIMEOUT, YtDlp};

/// Metadata resolution request.
#[derive(Debug, Clone, Default)]
pub struct FetchRequest {
    /// Located yt-dlp executable.
    pub tool: Option<YtDlp>,
    /// URL to resolve.
    pub url: String,
    /// Netscape cookie files passed to `yt-dlp`.
    pub cookies: Vec<(String, PathBuf)>,
    /// Extra arguments passed to `yt-dlp`.
    pub args: Vec<String>,
}

/// Metadata returned from `yt-dlp`.
#[derive(Debug, Clone)]
pub struct FetchResult {
    /// The parsed `yt-dlp` video metadata.
    pub video: yt_dlp::model::Video,
}

/// Resolves a URL through the managed `yt-dlp` process.
pub async fn fetch(request: FetchRequest) -> Result<FetchResult, Error> {
    let never = Arc::new(AtomicBool::new(false));
    let pid = Arc::new(AtomicU32::new(0));
    fetch_with_cancel(request, never, pid).await
}

/// Resolves a URL through the managed `yt-dlp` process, allowing cancellation.
pub async fn fetch_with_cancel(
    request: FetchRequest,
    cancel: Arc<AtomicBool>,
    pid: Arc<AtomicU32>,
) -> Result<FetchResult, Error> {
    let tool = request.tool.ok_or(Error::NotFound)?;
    let url = request.url.clone();
    let cookies = normalize_cookies(request.cookies);
    let args = normalize_args(request.args);
    let tool_for_task = tool.clone();
    let cancel_flag = cancel.clone();
    let pid_flag = pid.clone();

    app_logging::notice(format!("resolving url with yt-dlp: url={url}"));
    let task_url = url.clone();
    let result = tokio::task::spawn_blocking(move || {
        run_fetch(&tool_for_task, &task_url, &cookies, &args, &cancel_flag, &pid_flag)
    })
    .await
    .map_err(|error| Error::YtDlp(format!("yt-dlp task failed: {error}")))?;

    match result {
        Ok(video) => {
            app_logging::notice(format!(
                "resolved url with yt-dlp: url={url} title=\"{}\" formats={}",
                video.title,
                video.formats.len()
            ));
            Ok(FetchResult { video })
        }
        Err(error) => {
            app_logging::error(format!("yt-dlp failed to resolve url: url={url} error={error}"));
            Err(error)
        }
    }
}

fn run_fetch(
    tool: &YtDlp,
    url: &str,
    cookies: &[(String, PathBuf)],
    args: &[String],
    cancel: &AtomicBool,
    pid: &AtomicU32,
) -> Result<yt_dlp::model::Video, Error> {
    let mut command = tool.command();
    command.args(common_args(args));
    for (_, cookie) in cookies {
        command.arg("--cookies");
        command.arg(cookie);
    }
    command.arg("--dump-single-json");
    command.arg("--no-check-formats");
    command.arg("--");
    command.arg(url);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|error| {
        Error::YtDlp(format!(
            "yt-dlp could not be started for {}: {error}",
            tool.program().display()
        ))
    })?;
    pid.store(child.id(), Ordering::SeqCst);
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut out) = stdout {
            let _ = out.read_to_end(&mut bytes);
        }
        bytes
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut err) = stderr {
            let _ = err.read_to_end(&mut bytes);
        }
        bytes
    });

    let deadline = Instant::now() + RESOLUTION_TIMEOUT;
    let status = loop {
        if cancel.load(Ordering::SeqCst) {
            crate::locate::terminate_child_process(&mut child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            crate::locate::kill_all(tool);
            pid.store(0, Ordering::SeqCst);
            return Err(Error::YtDlp("yt-dlp request was cancelled".to_string()));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                crate::locate::terminate_child_process(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                crate::locate::kill_all(tool);
                pid.store(0, Ordering::SeqCst);
                return Err(Error::YtDlp(format!(
                    "yt-dlp did not respond within {} seconds",
                    RESOLUTION_TIMEOUT.as_secs()
                )));
            }
            Err(error) => {
                crate::locate::terminate_child_process(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                crate::locate::kill_all(tool);
                pid.store(0, Ordering::SeqCst);
                return Err(Error::YtDlp(format!("yt-dlp process failed: {error}")));
            }
        }
    };
    pid.store(0, Ordering::SeqCst);

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        let message = String::from_utf8_lossy(&stderr).trim().to_string();
        let message = if message.is_empty() {
            String::from_utf8_lossy(&stdout).trim().to_string()
        } else {
            message
        };
        let detail =
            if message.is_empty() { format!("yt-dlp failed ({})", status) } else { message };
        return Err(Error::YtDlp(detail));
    }

    let value: serde_json::Value = serde_json::from_slice(&stdout)
        .map_err(|error| Error::YtDlp(format!("yt-dlp returned invalid JSON: {error}")))?;
    deserialize_video_lenient(value)
        .map_err(|error| Error::YtDlp(format!("yt-dlp returned unexpected metadata: {error}")))
}

fn common_args(extra: &[String]) -> Vec<String> {
    let mut args = vec![
        "--no-warnings".to_string(),
        "--no-progress".to_string(),
        "--ignore-config".to_string(),
        "--socket-timeout".to_string(),
        RESOLUTION_TIMEOUT.as_secs().to_string(),
    ];
    for arg in extra {
        if !args.iter().any(|existing| existing == arg) {
            args.push(arg.clone());
        }
    }
    args
}

fn normalize_cookies(mut cookies: Vec<(String, PathBuf)>) -> Vec<(String, PathBuf)> {
    cookies.retain(|(_, path)| path.is_file());
    cookies.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    cookies.dedup();
    cookies
}

fn normalize_args(mut args: Vec<String>) -> Vec<String> {
    args.retain(|arg| !arg.trim().is_empty());
    args
}

fn dummy_ffmpeg_path(output_dir: &Path) -> PathBuf {
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    output_dir.join(name)
}

fn deserialize_video_lenient(
    value: serde_json::Value,
) -> Result<yt_dlp::model::Video, serde_json::Error> {
    match serde_json::from_value::<yt_dlp::model::Video>(value.clone()) {
        Ok(video) => Ok(video),
        Err(error) if error.to_string().contains("missing field") => {
            let normalized = normalize_video_value(value);
            serde_json::from_value(normalized)
        }
        Err(error) => Err(error),
    }
}

fn normalize_video_value(mut value: serde_json::Value) -> serde_json::Value {
    let Some(video) = value.as_object_mut() else {
        return value;
    };

    for (key, default) in [
        ("display_id", serde_json::Value::String(String::new())),
        ("tags", serde_json::Value::Array(Vec::new())),
        ("webpage_url_domain", serde_json::Value::String(String::new())),
        ("extractor", serde_json::Value::String(String::new())),
        ("extractor_key", serde_json::Value::String(String::new())),
        ("playlist", serde_json::Value::Null),
        ("playlist_index", serde_json::Value::Null),
        ("thumbnail", serde_json::Value::Null),
        ("duration_string", serde_json::Value::Null),
        ("release_year", serde_json::Value::Null),
        ("epoch", serde_json::Value::Null),
        ("asr", serde_json::Value::Null),
        ("filesize", serde_json::Value::Null),
        ("format_id", serde_json::Value::String(String::new())),
        ("format_note", serde_json::Value::Null),
        ("source_preference", serde_json::Value::Null),
        ("audio_channels", serde_json::Value::Null),
        ("quality", serde_json::Value::Null),
        ("has_drm", serde_json::Value::Null),
        ("tbr", serde_json::Value::Null),
        ("filesize_approx", serde_json::Value::Null),
        ("url", serde_json::Value::String(String::new())),
        ("language", serde_json::Value::Null),
        ("language_preference", serde_json::Value::Null),
        ("ext", serde_json::Value::String(String::new())),
        ("vcodec", serde_json::Value::String(String::new())),
        ("acodec", serde_json::Value::String(String::new())),
        ("dynamic_range", serde_json::Value::Null),
        ("downloader_options", serde_json::Value::Null),
        ("protocol", serde_json::Value::String("https".to_string())),
        ("video_ext", serde_json::Value::String(String::new())),
        ("audio_ext", serde_json::Value::String(String::new())),
        ("resolution", serde_json::Value::Null),
        ("aspect_ratio", serde_json::Value::Null),
        ("http_headers", serde_json::json!({})),
        ("_type", serde_json::Value::Null),
    ] {
        video.entry(key.to_string()).or_insert(default);
    }

    if let Some(formats) = video.get_mut("formats").and_then(|v| v.as_array_mut()) {
        for format in formats {
            let Some(obj) = format.as_object_mut() else {
                continue;
            };
            for (key, default) in [
                ("format_id", serde_json::Value::String(String::new())),
                ("url", serde_json::Value::String(String::new())),
                ("ext", serde_json::Value::String(String::new())),
                ("protocol", serde_json::Value::String("https".to_string())),
                ("preference", serde_json::Value::Null),
                ("quality", serde_json::Value::Null),
                ("has_drm", serde_json::Value::Null),
                ("language", serde_json::Value::Null),
                ("language_preference", serde_json::Value::Null),
                ("format_note", serde_json::Value::Null),
                ("filesize", serde_json::Value::Null),
                ("filesize_approx", serde_json::Value::Null),
                ("vcodec", serde_json::Value::String("none".to_string())),
                ("acodec", serde_json::Value::String("none".to_string())),
                ("dynamic_range", serde_json::Value::Null),
                ("downloader_options", serde_json::Value::Null),
                ("container", serde_json::Value::Null),
                ("video_ext", serde_json::Value::String("none".to_string())),
                ("audio_ext", serde_json::Value::String("none".to_string())),
                ("resolution", serde_json::Value::Null),
                ("aspect_ratio", serde_json::Value::Null),
                ("http_headers", serde_json::json!({})),
                ("source_preference", serde_json::Value::Null),
                ("audio_channels", serde_json::Value::Null),
                ("tbr", serde_json::Value::Null),
                ("asr", serde_json::Value::Null),
                ("fps", serde_json::Value::Null),
                ("height", serde_json::Value::Null),
                ("width", serde_json::Value::Null),
                ("vbr", serde_json::Value::Null),
                ("abr", serde_json::Value::Null),
            ] {
                obj.entry(key.to_string()).or_insert(default);
            }
        }
    }

    value
}

/// Updates the managed yt-dlp executable through the `yt-dlp` crate.
pub async fn update_managed(program: &Path, output_dir: &Path) -> Result<String, Error> {
    let ffmpeg = dummy_ffmpeg_path(output_dir);
    let libraries = yt_dlp::client::deps::Libraries::new(program.to_path_buf(), ffmpeg);
    let downloader = yt_dlp::DownloaderBuilder::new(libraries, output_dir.to_path_buf())
        .with_timeout(Duration::from_secs(120))
        .build()
        .await
        .map_err(map_ytdlp_error)?;
    downloader.update_downloader().await.map_err(map_ytdlp_error)?;
    let tool = crate::locate::at(program, crate::locate::ToolPaths::new(output_dir));
    tool.version()
}

fn map_ytdlp_error(error: yt_dlp::error::Error) -> Error {
    match error {
        yt_dlp::error::Error::CommandFailed { command, exit_code, stderr } => {
            Error::YtDlp(format!("{command} failed ({exit_code}): {stderr}"))
        }
        yt_dlp::error::Error::Timeout { operation, duration } => {
            Error::YtDlp(format!("{operation} timed out after {}s", duration.as_secs()))
        }
        yt_dlp::error::Error::VideoFetch { url, reason } => {
            Error::YtDlp(format!("failed to fetch {url}: {reason}"))
        }
        other => Error::YtDlp(other.to_string()),
    }
}

/// Detects whether an error indicates yt-dlp should be updated.
pub fn is_update_required(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "update yt-dlp",
        "yt-dlp is outdated",
        "outdated",
        "unable to extract",
        "nsig extraction failed",
        "signature extraction failed",
        "requested format is not available",
        "this app needs to be updated",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}
