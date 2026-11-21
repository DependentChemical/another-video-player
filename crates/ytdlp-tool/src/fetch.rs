use std::path::{Path, PathBuf};
use std::time::Duration;

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

/// Resolves a URL through the `yt-dlp` crate.
pub async fn fetch(request: FetchRequest) -> Result<FetchResult, Error> {
    let tool = request.tool.ok_or(Error::NotFound)?;
    let url = request.url.clone();
    let cookies = normalize_cookies(request.cookies);
    let args = normalize_args(request.args);
    let downloader = build_downloader(&tool, &cookies, &args).await?;

    app_logging::notice(format!("resolving url with yt-dlp crate: url={url}"));
    let result = downloader.fetch_video_infos(&url).await;
    match result {
        Ok(video) => {
            app_logging::notice(format!(
                "resolved url with yt-dlp crate: url={url} title=\"{}\" formats={}",
                video.title,
                video.formats.len()
            ));
            Ok(FetchResult { video })
        }
        Err(error) => {
            let mapped = map_ytdlp_error(error);
            app_logging::error(format!(
                "yt-dlp crate failed to resolve url: url={url} error={mapped}"
            ));
            Err(mapped)
        }
    }
}

async fn build_downloader(
    tool: &YtDlp,
    cookies: &[(String, PathBuf)],
    args: &[String],
) -> Result<yt_dlp::Downloader, Error> {
    let output_dir = tool.paths().tools_output_dir();
    let ffmpeg = dummy_ffmpeg_path(&output_dir);
    let libraries = yt_dlp::client::deps::Libraries::new(tool.program().to_path_buf(), ffmpeg);

    let mut builder = yt_dlp::DownloaderBuilder::new(libraries, output_dir);
    builder = builder.with_timeout(RESOLUTION_TIMEOUT);
    builder = builder.with_args(common_args(args));
    if let Some((_, cookie)) = cookies.first() {
        builder = builder.with_cookies(cookie.clone());
    }

    let mut downloader = builder.build().await.map_err(map_ytdlp_error)?;
    for (_, cookie) in cookies.iter().skip(1) {
        let arg = format!("--cookies={}", cookie.display());
        if !downloader.args().iter().any(|existing| existing == &arg) {
            downloader.add_arg(arg);
        }
    }
    Ok(downloader)
}

fn common_args(extra: &[String]) -> Vec<String> {
    let mut args = vec![
        "--no-warnings".to_string(),
        "--no-progress".to_string(),
        "--ignore-config".to_string(),
        "--socket-timeout".to_string(),
        "15".to_string(),
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
