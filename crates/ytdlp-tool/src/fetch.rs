use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::{Error, RESOLUTION_TIMEOUT, SOCKET_TIMEOUT, YtDlp};

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
    let stdout_tail: OutputTail = Arc::new(Mutex::new(Vec::new()));
    let stderr_tail: OutputTail = Arc::new(Mutex::new(Vec::new()));
    let stdout_reader = spawn_tail_reader(child.stdout.take(), Arc::clone(&stdout_tail));
    let stderr_reader = spawn_tail_reader(child.stderr.take(), Arc::clone(&stderr_tail));

    let started = Instant::now();
    let deadline = started + RESOLUTION_TIMEOUT;
    let mut socket_timeout_logged = false;
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
            Ok(None) => {
                let elapsed = started.elapsed();
                if !socket_timeout_logged && elapsed >= SOCKET_TIMEOUT {
                    socket_timeout_logged = true;
                    app_logging::warning(format!(
                        "yt-dlp exceeded socket timeout without exiting: url={url} socket_timeout={}s elapsed={}s hard_timeout={}s stderr_tail=\"{}\" stdout_tail=\"{}\"",
                        SOCKET_TIMEOUT.as_secs(),
                        elapsed.as_secs(),
                        RESOLUTION_TIMEOUT.as_secs(),
                        output_excerpt(&stderr_tail),
                        output_excerpt(&stdout_tail),
                    ));
                }
                if Instant::now() >= deadline {
                    let stderr_text = tail_text(&stderr_tail);
                    let stdout_text = tail_text(&stdout_tail);
                    let stderr_excerpt = last_chars(&stderr_text, 300);
                    let stdout_excerpt = last_chars(&stdout_text, 300);
                    crate::locate::terminate_child_process(&mut child);
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    app_logging::warning(format!(
                        "yt-dlp hard timeout reached; killing process: url={url} socket_timeout={}s elapsed={}s hard_timeout={}s stderr_len={} stdout_len={} stderr_tail=\"{}\" stdout_tail=\"{}\"",
                        SOCKET_TIMEOUT.as_secs(),
                        elapsed.as_secs(),
                        RESOLUTION_TIMEOUT.as_secs(),
                        stderr_text.len(),
                        stdout_text.len(),
                        stderr_excerpt,
                        stdout_excerpt
                    ));
                    crate::locate::kill_all(tool);
                    pid.store(0, Ordering::SeqCst);
                    return Err(Error::YtDlp(format!(
                        "yt-dlp did not respond within {} seconds; last output: stderr=\"{}\" stdout=\"{}\"",
                        RESOLUTION_TIMEOUT.as_secs(),
                        stderr_excerpt,
                        stdout_excerpt
                    )));
                }
                thread::sleep(Duration::from_millis(50));
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
    let _ = stdout_reader.join();
    let _ = stderr_reader.join();
    let stdout = tail_text(&stdout_tail);
    let stderr = tail_text(&stderr_tail);

    if !status.success() {
        let message = if stderr.is_empty() { stdout } else { stderr };
        let detail =
            if message.is_empty() { format!("yt-dlp failed ({})", status) } else { message };
        return Err(Error::YtDlp(detail));
    }

    let value: serde_json::Value = serde_json::from_str(&stdout)
        .map_err(|error| Error::YtDlp(format!("yt-dlp returned invalid JSON: {error}")))?;
    deserialize_video_lenient(value)
        .map_err(|error| Error::YtDlp(format!("yt-dlp returned unexpected metadata: {error}")))
}

/// Live tail of one of `yt-dlp`'s output streams, kept while the stream is read.
type OutputTail = Arc<Mutex<Vec<u8>>>;

/// Maximum bytes kept of each output stream.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// The trimmed text of the output stream's tail.
fn tail_text(tail: &OutputTail) -> String {
    let guard = tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    String::from_utf8_lossy(guard.as_slice()).trim().to_string()
}

/// The final part of the output stream's text, limited to its last 300 characters.
fn output_excerpt(tail: &OutputTail) -> String {
    last_chars(&tail_text(tail), 300)
}

/// The last `count` characters of `text`, in their original order.
fn last_chars(text: &str, count: usize) -> String {
    text.chars().rev().take(count).collect::<String>().chars().rev().collect()
}

/// Reads `stream` into `tail` until it ends, keeping only its final bytes.
fn spawn_tail_reader<R: Read + Send + 'static>(
    stream: Option<R>,
    tail: OutputTail,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        if let Some(mut stream) = stream {
            let mut buffer = [0u8; 4096];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut guard =
                            tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        guard.extend_from_slice(&buffer[..n]);
                        if guard.len() > MAX_OUTPUT_BYTES {
                            let start = guard.len() - MAX_OUTPUT_BYTES;
                            guard.copy_within(start.., 0);
                            guard.truncate(MAX_OUTPUT_BYTES);
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    })
}

fn common_args(extra: &[String]) -> Vec<String> {
    let mut args = vec![
        "--no-warnings".to_string(),
        "--no-progress".to_string(),
        "--ignore-config".to_string(),
        "--socket-timeout".to_string(),
        SOCKET_TIMEOUT.as_secs().to_string(),
        "-v".to_string(),
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

/// Maximum number of repair passes before giving up.
const MAX_MISSING_FIELD_REPAIRS: usize = 32;

/// Parses `yt-dlp` video metadata, tolerating extractors that omit fields the
/// `yt-dlp` model marks as required.
///
/// Extractors from any source occasionally return metadata without fields the
/// model needs, so a missing-field error triggers a retry with the field
/// filled in; the next error's missing-field name is extracted and repaired
/// in turn, until the document parses or a real type error surfaces.
fn deserialize_video_lenient(
    value: serde_json::Value,
) -> Result<yt_dlp::model::Video, serde_json::Error> {
    let mut value = value;
    for _ in 0..MAX_MISSING_FIELD_REPAIRS {
        let error = match serde_json::from_value::<yt_dlp::model::Video>(value.clone()) {
            Ok(video) => return Ok(video),
            Err(error) => error,
        };
        let Some(field) = missing_field_name(&error) else {
            return Err(error);
        };
        if !try_fills(&mut value, &field) {
            return Err(error);
        }
    }
    serde_json::from_value::<yt_dlp::model::Video>(value)
}

/// Extracts the field name from a serde missing-field error message.
fn missing_field_name(error: &serde_json::Error) -> Option<String> {
    let message = error.to_string();
    let marker = "missing field `";
    let start = message.find(marker)? + marker.len();
    let end = message[start..].find('`')? + start;
    let name = &message[start..end];
    (!name.is_empty()).then(|| name.to_string())
}

/// Tries filling `field`'s typed default into the objects lacking it, in
/// tree order, until the tree parses or the candidates run out.
///
/// A fill is kept only when the next parse reports a different missing field;
/// fills that break the tree (structs that absorb unknown keys with other
/// types, the flattened `HttpHeaders.extra` map, for example) are undone and
/// the next lacking object is tried. Returns whether any fill was kept.
fn try_fills(value: &mut serde_json::Value, field: &str) -> bool {
    let mut kept = false;
    for path in lacking_paths(value, field) {
        let default = missing_field_default(field);
        set_lacking(value, &path, field, default);
        match serde_json::from_value::<yt_dlp::model::Video>(value.clone()) {
            Ok(_) => return true,
            Err(error) if missing_field_name(&error).is_some() => kept = true,
            Err(_) => remove_at(value, &path, field),
        }
        if kept {
            break;
        }
    }
    kept
}

/// A step from the root of the value tree to the object it identifies.
#[derive(Clone)]
enum PathSeg {
    /// An object member key.
    Key(String),
    /// An array element index.
    Index(usize),
}

/// Paths to every object in the tree that lacks `field`, in pre-order.
fn lacking_paths(value: &serde_json::Value, field: &str) -> Vec<Vec<PathSeg>> {
    let mut paths = Vec::new();
    collect_lacking(value, field, &mut Vec::new(), &mut paths);
    paths
}

/// Records the path of every object that lacks `field`, recursing into the
/// values of objects that lack it themselves.
fn collect_lacking(
    value: &serde_json::Value,
    field: &str,
    path: &mut Vec<PathSeg>,
    paths: &mut Vec<Vec<PathSeg>>,
) {
    match value {
        serde_json::Value::Object(object) => {
            if !object.contains_key(field) {
                paths.push(path.clone());
            }
            for (key, nested) in object {
                path.push(PathSeg::Key(key.clone()));
                collect_lacking(nested, field, path, paths);
                path.pop();
            }
        }
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                path.push(PathSeg::Index(index));
                collect_lacking(item, field, path, paths);
                path.pop();
            }
        }
        _ => {}
    }
}

/// Inserts `field` with `default` into the object at `path`.
fn set_lacking(
    value: &mut serde_json::Value,
    path: &[PathSeg],
    field: &str,
    default: serde_json::Value,
) {
    let Some((seg, rest)) = path.split_first() else {
        if let serde_json::Value::Object(object) = value {
            object.insert(field.to_string(), default);
        }
        return;
    };
    match seg {
        PathSeg::Key(key) => {
            if let serde_json::Value::Object(object) = value
                && let Some(nested) = object.get_mut(key)
            {
                set_lacking(nested, rest, field, default);
            }
        }
        PathSeg::Index(index) => {
            if let serde_json::Value::Array(items) = value
                && let Some(nested) = items.get_mut(*index)
            {
                set_lacking(nested, rest, field, default);
            }
        }
    }
}

/// Removes `field` from the object at `path`.
fn remove_at(value: &mut serde_json::Value, path: &[PathSeg], field: &str) {
    let Some((seg, rest)) = path.split_first() else {
        if let serde_json::Value::Object(object) = value {
            object.remove(field);
        }
        return;
    };
    match seg {
        PathSeg::Key(key) => {
            if let serde_json::Value::Object(object) = value
                && let Some(nested) = object.get_mut(key)
            {
                remove_at(nested, rest, field);
            }
        }
        PathSeg::Index(index) => {
            if let serde_json::Value::Array(items) = value
                && let Some(nested) = items.get_mut(*index)
            {
                remove_at(nested, rest, field);
            }
        }
    }
}

/// The value inserted when a required field is missing. `Null` deserializes
/// as an absent `Option`; the names below map to required fields of the
/// `yt-dlp` model, where null or empty is not accepted.
fn missing_field_default(field: &str) -> serde_json::Value {
    match field {
        // `Thumbnail.preference` is a required integer, as was `Format`'s
        // in earlier model versions; extractors omit it occasionally.
        "preference" => serde_json::json!(0),
        "age_limit" => serde_json::json!(0),
        "playable_in_embed" => serde_json::json!(false),
        "id" | "title" | "live_status" | "extractor" | "extractor_key" | "url" | "format"
        | "format_id" | "ext" | "extension" | "version" | "repository" => {
            serde_json::json!("")
        }
        "_version" => serde_json::json!({ "version": "", "repository": "" }),
        _ => serde_json::Value::Null,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn well_formed_video() -> serde_json::Value {
        json!({
            "id": "abc123",
            "title": "Some video",
            "age_limit": 0,
            "live_status": "finished",
            "playable_in_embed": true,
            "extractor": "e",
            "extractor_key": "e",
            "_version": { "version": "2026.01.01", "repository": "yt-dlp/yt-dlp" },
            "formats": [
                {
                    "format": "HLS Audio",
                    "format_id": "hls",
                    "url": "https://example.invalid/hls",
                    "ext": "mp4",
                    "http_headers": {}
                }
            ]
        })
    }

    #[test]
    fn parses_well_formed_metadata() {
        let video = deserialize_video_lenient(well_formed_video()).expect("metadata parses");
        assert_eq!(video.id, "abc123");
        assert_eq!(video.title, "Some video");
        assert_eq!(video.formats.len(), 1);
    }

    #[test]
    fn fills_missing_thumbnail_preference() {
        // Extractors of other sources omit `preference` on thumbnail entries.
        let mut video = well_formed_video();
        video["thumbnails"] = json!([{ "id": "t1", "url": "https://example.invalid/t1.jpg" }]);

        let parsed = deserialize_video_lenient(video).expect("metadata parses");
        assert_eq!(parsed.thumbnails[0].preference, 0);
    }

    #[test]
    fn fills_missing_required_top_level_fields() {
        let mut video = well_formed_video();
        let object = video.as_object_mut().expect("video is an object");
        for key in [
            "id",
            "title",
            "age_limit",
            "live_status",
            "playable_in_embed",
            "extractor",
            "extractor_key",
        ] {
            object.remove(key);
        }

        let parsed = deserialize_video_lenient(video).expect("metadata parses");
        assert_eq!(parsed.id, "");
        assert_eq!(parsed.age_limit, 0);
        assert!(parsed.live_status.is_empty());
    }

    #[test]
    fn rejects_documents_of_the_wrong_shape() {
        assert!(deserialize_video_lenient(json!({ "id": 1 })).is_err());
    }

    #[test]
    fn tail_reader_records_the_stream_output() {
        let tail: OutputTail = Arc::new(Mutex::new(Vec::new()));
        let handle = spawn_tail_reader(
            Some(std::io::Cursor::new(b"line1\nline2\n".to_vec())),
            Arc::clone(&tail),
        );
        let _ = handle.join();
        assert_eq!(tail_text(&tail), "line1\nline2");
    }

    #[test]
    fn tail_reader_keeps_only_the_final_bytes() {
        let tail: OutputTail = Arc::new(Mutex::new(Vec::new()));
        let handle = spawn_tail_reader(
            Some(std::io::Cursor::new(vec![b'a'; MAX_OUTPUT_BYTES + 100])),
            Arc::clone(&tail),
        );
        let _ = handle.join();
        assert_eq!(tail.lock().unwrap().len(), MAX_OUTPUT_BYTES);
    }

    #[test]
    fn tail_text_trims_the_stream_contents() {
        let tail: OutputTail = Arc::new(Mutex::new(b"  hello\n\n".to_vec()));
        assert_eq!(tail_text(&tail), "hello");
    }

    #[test]
    fn output_excerpt_is_bounded_to_300_characters() {
        let tail: OutputTail = Arc::new(Mutex::new(vec![b'x'; 1000]));
        assert_eq!(output_excerpt(&tail).chars().count(), 300);
    }
}
