use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::{Error, ProxyOutcome, ProxyPool, SOCKET_TIMEOUT, YtDlp};

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
    /// Optional proxy pool used for yt-dlp network requests.
    pub proxy_pool: Option<Arc<ProxyPool>>,
}

/// Metadata returned from `yt-dlp`.
#[derive(Debug, Clone)]
pub struct FetchResult {
    /// The parsed `yt-dlp` document.
    pub document: FetchDocument,
}

/// The document `yt-dlp` returned for one URL.
#[derive(Debug, Clone)]
pub enum FetchDocument {
    /// A single video or streamable media.
    Video(Box<yt_dlp::model::Video>),
    /// A playlist containing entries.
    Playlist(Box<yt_dlp::model::playlist::Playlist>),
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
    let proxy_pool = request.proxy_pool.clone();
    let tool_for_task = tool.clone();
    let cancel_flag = cancel.clone();
    let pid_flag = pid.clone();

    app_logging::notice(format!("resolving url with yt-dlp: url={url}"));
    let task_url = url.clone();
    let budget = Budget::start();
    let result = tokio::task::spawn_blocking(move || {
        run_fetch(
            &tool_for_task,
            &task_url,
            &cookies,
            &args,
            proxy_pool.as_deref(),
            &cancel_flag,
            &pid_flag,
            budget,
        )
    })
    .await
    .map_err(|error| Error::YtDlp(format!("yt-dlp task failed: {error}")))?;

    match result {
        Ok(document) => {
            match &document {
                FetchDocument::Video(video) => app_logging::notice(format!(
                    "resolved url with yt-dlp: url={url} title=\"{}\" formats={}",
                    video.title,
                    video.formats.len()
                )),
                FetchDocument::Playlist(playlist) => app_logging::notice(format!(
                    "resolved playlist with yt-dlp: url={url} title=\"{}\" entries={}",
                    playlist.title,
                    playlist.entries.len()
                )),
            }
            Ok(FetchResult { document })
        }
        Err(error) => {
            app_logging::error(format!("yt-dlp failed to resolve url: url={url} error={error}"));
            Err(error)
        }
    }
}

/// The timing state of one `yt-dlp` invocation.
#[derive(Debug, Clone, Copy)]
struct Budget {
    /// The moment the invocation started.
    started: Instant,
}

impl Budget {
    /// Starts a new invocation timer.
    fn start() -> Self {
        Self { started: Instant::now() }
    }

    /// Whether the request may still start a process.
    fn may_start(&self, cancel: &AtomicBool) -> Result<(), Error> {
        if cancel.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn run_fetch(
    tool: &YtDlp,
    url: &str,
    cookies: &[(String, PathBuf)],
    args: &[String],
    proxy_pool: Option<&ProxyPool>,
    cancel: &AtomicBool,
    pid: &AtomicU32,
    budget: Budget,
) -> Result<FetchDocument, Error> {
    // A request that is cancelled before it starts never spawns a process.
    budget.may_start(cancel)?;

    let proxy = proxy_pool.and_then(|pool| pool.proxy_for(url));
    let mut command = tool.command();
    command.args(common_args(args, proxy.as_deref()));
    for (_, cookie) in cookies {
        command.arg("--cookies");
        command.arg(cookie);
    }
    command.arg("--dump-single-json");
    command.arg("--no-check-formats");
    command.arg("--");
    command.arg(url);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(proxy) = proxy.as_deref() {
        app_logging::notice(format!("yt-dlp using proxy: url={url} proxy={proxy}"));
    }

    let mut child = command.spawn().map_err(|error| {
        Error::YtDlp(format!(
            "yt-dlp could not be started for {}: {error}",
            tool.program().display()
        ))
    })?;
    pid.store(child.id(), Ordering::SeqCst);
    // The document is the whole stdout, so it is collected in full; only
    // stderr is tailed for diagnostics.
    let stdout_capture: OutputTail = Arc::new(Mutex::new(Vec::new()));
    let stderr_capture: OutputTail = Arc::new(Mutex::new(Vec::new()));
    let stdout_reader = spawn_output_reader(child.stdout.take(), Arc::clone(&stdout_capture), None);
    let stderr_reader = spawn_output_reader(
        child.stderr.take(),
        Arc::clone(&stderr_capture),
        Some(MAX_OUTPUT_BYTES),
    );

    let mut socket_timeout_logged = false;
    let status = loop {
        if cancel.load(Ordering::SeqCst) {
            crate::locate::terminate_child_process(&mut child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            pid.store(0, Ordering::SeqCst);
            // Only this process is killed. A run that is cancelled must not
            // take down the processes of the work that replaced it.
            return Err(Error::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let elapsed = budget.started.elapsed();
                if !socket_timeout_logged && elapsed >= SOCKET_TIMEOUT {
                    socket_timeout_logged = true;
                    app_logging::warning(format!(
                        "yt-dlp exceeded socket timeout without exiting: url={url} socket_timeout={}s elapsed={}s stderr_tail=\"{}\" stdout_tail=\"{}\"",
                        SOCKET_TIMEOUT.as_secs(),
                        elapsed.as_secs(),
                        output_excerpt(&stderr_capture),
                        output_excerpt(&stdout_capture),
                    ));
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                crate::locate::terminate_child_process(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                pid.store(0, Ordering::SeqCst);
                return Err(Error::YtDlp(format!("yt-dlp process failed: {error}")));
            }
        }
    };
    pid.store(0, Ordering::SeqCst);
    let _ = stdout_reader.join();
    let _ = stderr_reader.join();
    let stdout = tail_text(&stdout_capture);
    let stderr = tail_text(&stderr_capture);

    if !status.success() {
        let message = if stderr.is_empty() { output_excerpt(&stdout_capture) } else { stderr };
        let detail =
            if message.is_empty() { format!("yt-dlp failed ({})", status) } else { message };
        if let (Some(pool), Some(proxy)) = (proxy_pool, proxy.as_deref()) {
            pool.report(proxy, ProxyOutcome::Failure);
        }
        return Err(Error::YtDlp(detail));
    }

    let value = match parse_document(&stdout) {
        Ok(value) => value,
        Err(error) => {
            if let (Some(pool), Some(proxy)) = (proxy_pool, proxy.as_deref()) {
                pool.report(proxy, ProxyOutcome::Failure);
            }
            return Err(Error::YtDlp(format!("yt-dlp returned invalid JSON: {error}")));
        }
    };
    let document = match deserialize_document_lenient(value) {
        Ok(document) => document,
        Err(error) => {
            if let (Some(pool), Some(proxy)) = (proxy_pool, proxy.as_deref()) {
                pool.report(proxy, ProxyOutcome::Failure);
            }
            return Err(Error::YtDlp(format!("yt-dlp returned unexpected metadata: {error}")));
        }
    };
    if let (Some(pool), Some(proxy)) = (proxy_pool, proxy.as_deref()) {
        pool.report(proxy, ProxyOutcome::Success);
    }
    Ok(document)
}

/// Parses the first JSON value in `stdout`.
///
/// The output is the whole `--dump-single-json` document; a value is taken
/// where one parses and anything after it is ignored, since `yt-dlp` may
/// print stray text around the document.
fn parse_document(stdout: &str) -> Result<serde_json::Value, String> {
    let mut values = serde_json::Deserializer::from_str(stdout).into_iter::<serde_json::Value>();
    match values.next() {
        Some(Ok(value)) => Ok(value),
        Some(Err(error)) => Err(error.to_string()),
        None => Err("no JSON value in output".to_string()),
    }
}

/// Captured output of one of `yt-dlp`'s output streams, kept while the stream
/// is read; tail reads are capped at [`MAX_OUTPUT_BYTES`] and the stdout
/// document is collected without a cap.
type OutputTail = Arc<Mutex<Vec<u8>>>;

/// Maximum bytes kept of each diagnostic output stream.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// The trimmed text of the captured output.
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

/// Reads `stream` until it ends into `capture`, keeping its final `max_bytes`
/// when given or every byte when `None`.
fn spawn_output_reader<R: Read + Send + 'static>(
    stream: Option<R>,
    capture: OutputTail,
    max_bytes: Option<usize>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        if let Some(mut stream) = stream {
            let mut buffer = [0u8; 4096];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut guard =
                            capture.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        guard.extend_from_slice(&buffer[..n]);
                        if let Some(max) = max_bytes
                            && guard.len() > max
                        {
                            let start = guard.len() - max;
                            guard.copy_within(start.., 0);
                            guard.truncate(max);
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    })
}

fn common_args(extra: &[String], proxy: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "--no-warnings".to_string(),
        "--no-progress".to_string(),
        "--ignore-config".to_string(),
        "-v".to_string(),
    ];
    let mut has_socket_timeout = false;
    let mut has_proxy = false;
    let mut iter = extra.iter();
    while let Some(arg) = iter.next() {
        if !args.iter().any(|existing| existing == arg) {
            args.push(arg.clone());
        }
        if arg == "--socket-timeout" {
            has_socket_timeout = true;
            if let Some(value) = iter.next()
                && !args.iter().any(|existing| existing == value)
            {
                args.push(value.clone());
            }
        }
        if arg == "--proxy" {
            has_proxy = true;
            if let Some(value) = iter.next()
                && !args.iter().any(|existing| existing == value)
            {
                args.push(value.clone());
            }
        }
    }
    if !has_socket_timeout {
        args.insert(3, SOCKET_TIMEOUT.as_secs().to_string());
        args.insert(3, "--socket-timeout".to_string());
    }
    if !has_proxy && let Some(proxy) = proxy {
        let proxy = proxy.trim();
        if !proxy.is_empty() {
            args.push("--proxy".to_string());
            args.push(proxy.to_string());
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

/// Parses `yt-dlp` metadata, tolerating missing fields in both video and
/// playlist documents.
fn deserialize_document_lenient(
    mut value: serde_json::Value,
) -> Result<FetchDocument, serde_json::Error> {
    if is_playlist_document(&value) {
        normalize_playlist_thumbnails(&mut value);
        let playlist = deserialize_lenient::<yt_dlp::model::playlist::Playlist>(value)?;
        Ok(FetchDocument::Playlist(Box::new(playlist)))
    } else {
        let video = deserialize_lenient::<yt_dlp::model::Video>(value)?;
        Ok(FetchDocument::Video(Box::new(video)))
    }
}

fn normalize_playlist_thumbnails(value: &mut serde_json::Value) {
    let Some(entries) = value.get_mut("entries").and_then(serde_json::Value::as_array_mut) else {
        return;
    };
    for entry in entries {
        let Some(obj) = entry.as_object_mut() else { continue };
        let has_thumbnail = obj
            .get("thumbnail")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if has_thumbnail {
            continue;
        }
        let thumb = obj.get("thumbnails").and_then(serde_json::Value::as_array).and_then(|arr| {
            arr.iter().rev().find_map(|t| {
                t.get("url")
                    .or_else(|| t.get("src"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string)
            })
        });
        if let Some(thumb) = thumb {
            obj.insert("thumbnail".to_string(), serde_json::Value::String(thumb));
        }
    }
}

/// Whether the JSON document describes a playlist.
fn is_playlist_document(value: &serde_json::Value) -> bool {
    value
        .get("_type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("playlist"))
        || value.get("entries").is_some_and(serde_json::Value::is_array)
}

/// Parses metadata, tolerating extractors that omit fields the model marks as
/// required.
///
/// A missing-field error triggers a retry with the field filled in; the next
/// error's missing-field name is extracted and repaired in turn, until the
/// document parses or a real type error surfaces.
fn deserialize_lenient<T>(mut value: serde_json::Value) -> Result<T, serde_json::Error>
where
    T: serde::de::DeserializeOwned,
{
    for _ in 0..MAX_MISSING_FIELD_REPAIRS {
        let error = match serde_json::from_value::<T>(value.clone()) {
            Ok(parsed) => return Ok(parsed),
            Err(error) => error,
        };
        let Some(field) = missing_field_name(&error) else {
            return Err(error);
        };
        if !try_fills::<T>(&mut value, &field) {
            return Err(error);
        }
    }
    serde_json::from_value::<T>(value)
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
fn try_fills<T>(value: &mut serde_json::Value, field: &str) -> bool
where
    T: serde::de::DeserializeOwned,
{
    let mut kept = false;
    for path in lacking_paths(value, field) {
        let default = missing_field_default(field);
        set_lacking(value, &path, field, default);
        match serde_json::from_value::<T>(value.clone()) {
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
        let document = deserialize_document_lenient(well_formed_video()).expect("metadata parses");
        match document {
            FetchDocument::Video(video) => {
                assert_eq!(video.id, "abc123");
                assert_eq!(video.title, "Some video");
                assert_eq!(video.formats.len(), 1);
            }
            FetchDocument::Playlist(_) => panic!("expected video document"),
        }
    }

    #[test]
    fn fills_missing_thumbnail_preference() {
        let mut video = well_formed_video();
        video["thumbnails"] = json!([{ "id": "t1", "url": "https://example.invalid/t1.jpg" }]);

        let document = deserialize_document_lenient(video).expect("metadata parses");
        let FetchDocument::Video(parsed) = document else { panic!("expected video") };
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

        let document = deserialize_document_lenient(video).expect("metadata parses");
        let FetchDocument::Video(parsed) = document else { panic!("expected video") };
        assert_eq!(parsed.id, "");
        assert_eq!(parsed.age_limit, 0);
        assert!(parsed.live_status.is_empty());
    }

    #[test]
    fn rejects_documents_of_the_wrong_shape() {
        assert!(deserialize_document_lenient(json!({ "id": 1 })).is_err());
    }

    #[test]
    fn parses_playlist_documents() {
        let playlist = json!({
            "_type": "playlist",
            "id": "PL1",
            "title": "Playlist",
            "entries": [
                { "id": "a", "title": "A", "url": "https://example.com/a" },
                { "id": "b", "title": "B", "url": "https://example.com/b" }
            ]
        });
        let document = deserialize_document_lenient(playlist).expect("playlist parses");
        let FetchDocument::Playlist(parsed) = document else { panic!("expected playlist") };
        assert_eq!(parsed.id, "PL1");
        assert_eq!(parsed.entries.len(), 2);
    }

    #[test]
    fn flat_playlist_entry_thumbnail_is_normalized() {
        let playlist = json!({
            "_type": "playlist",
            "id": "PL1",
            "title": "Playlist",
            "entries": [
                {
                    "id": "a",
                    "title": "A",
                    "url": "https://example.com/a",
                    "thumbnails": [
                        { "url": "https://img/small.jpg", "width": 120 },
                        { "url": "https://img/large.jpg", "width": 480 }
                    ]
                }
            ]
        });
        let document = deserialize_document_lenient(playlist).expect("playlist parses");
        let FetchDocument::Playlist(parsed) = document else { panic!("expected playlist") };
        assert_eq!(parsed.entries[0].thumbnail.as_deref(), Some("https://img/large.jpg"));
    }

    #[test]
    fn output_reader_records_the_stream_output() {
        let tail: OutputTail = Arc::new(Mutex::new(Vec::new()));
        let handle = spawn_output_reader(
            Some(std::io::Cursor::new(b"line1\nline2\n".to_vec())),
            Arc::clone(&tail),
            Some(MAX_OUTPUT_BYTES),
        );
        let _ = handle.join();
        assert_eq!(tail_text(&tail), "line1\nline2");
    }

    #[test]
    fn output_reader_keeps_the_whole_document() {
        let capture: OutputTail = Arc::new(Mutex::new(Vec::new()));
        let handle = spawn_output_reader(
            Some(std::io::Cursor::new(vec![b'a'; MAX_OUTPUT_BYTES + 1024])),
            Arc::clone(&capture),
            None,
        );
        let _ = handle.join();
        assert_eq!(capture.lock().unwrap().len(), MAX_OUTPUT_BYTES + 1024);
    }

    #[test]
    fn output_reader_keeps_only_the_final_bytes() {
        let tail: OutputTail = Arc::new(Mutex::new(Vec::new()));
        let handle = spawn_output_reader(
            Some(std::io::Cursor::new(vec![b'a'; MAX_OUTPUT_BYTES + 100])),
            Arc::clone(&tail),
            Some(MAX_OUTPUT_BYTES),
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

    #[test]
    fn parses_documents_larger_than_the_tail_buffer() {
        let mut video = well_formed_video();
        video["title"] = json!("t".repeat(MAX_OUTPUT_BYTES + 1024));
        let text = serde_json::to_string(&video).expect("video serializes");
        assert!(text.len() > MAX_OUTPUT_BYTES);

        let parsed = deserialize_document_lenient(parse_document(&text).expect("document parses"))
            .expect("metadata parses");
        let FetchDocument::Video(video) = parsed else { panic!("expected video") };
        assert_eq!(video.title.len(), MAX_OUTPUT_BYTES + 1024);
    }

    #[test]
    fn parse_document_takes_the_first_value_and_ignores_trailing_text() {
        let text = serde_json::to_string(&well_formed_video()).expect("video serializes");
        assert!(parse_document(&format!("{text} trailing text")).is_ok());
    }

    #[test]
    fn common_args_include_socket_timeout_once() {
        let args = common_args(
            &["-4".to_string(), "--socket-timeout".to_string(), "20".to_string()],
            None,
        );
        assert_eq!(args.iter().filter(|a| a.as_str() == "--socket-timeout").count(), 1);
        assert!(args.windows(2).any(|w| w[0] == "--socket-timeout" && w[1] == "20"));
        assert!(args.iter().any(|a| a == "-4"));
    }

    #[test]
    fn common_args_add_proxy_when_requested() {
        let args = common_args(&[], Some("socks5://127.0.0.1:1080"));
        assert!(args.windows(2).any(|w| w[0] == "--proxy" && w[1] == "socks5://127.0.0.1:1080"));
    }

    #[test]
    fn common_args_keep_explicit_proxy() {
        let args = common_args(
            &["--proxy".to_string(), "http://1.2.3.4:8080".to_string()],
            Some("socks5://127.0.0.1:1080"),
        );
        assert_eq!(args.iter().filter(|a| a.as_str() == "--proxy").count(), 1);
        assert!(args.windows(2).any(|w| w[0] == "--proxy" && w[1] == "http://1.2.3.4:8080"));
    }

    #[test]
    fn parse_document_rejects_missing_values() {
        assert!(parse_document("").is_err());
        assert!(parse_document("garbage in front of the document").is_err());
    }

    #[test]
    fn a_cancelled_request_never_starts_a_process() {
        let cancel = AtomicBool::new(true);

        let error = Budget::start().may_start(&cancel).expect_err("a cancelled request is refused");

        assert!(matches!(error, Error::Cancelled));
    }

    #[test]
    fn a_request_not_cancelled_may_start() {
        let cancel = AtomicBool::new(false);

        assert!(Budget::start().may_start(&cancel).is_ok());
    }
}
