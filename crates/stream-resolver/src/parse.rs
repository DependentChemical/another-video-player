//! Turning `yt-dlp` crate metadata into a [`MediaSource`].

use std::collections::BTreeMap;

use media_model::{AudioStream, MediaSource, VideoStream};

use crate::{Error, ResolvedPlaylist, ResolvedPlaylistEntry};

/// Converts a `yt-dlp` crate video into Flatline's media model.
pub fn parse_video(video: yt_dlp::model::Video) -> Result<MediaSource, Error> {
    let mut video_streams = Vec::new();
    let mut audio_streams = Vec::new();

    for format in video.formats {
        let Some(url) = format.download_info.url.clone().filter(|url| !url.is_empty()) else {
            continue;
        };
        let ext = format.download_info.ext.as_str().to_string();
        if ext.eq_ignore_ascii_case("mhtml") {
            continue;
        }

        let format_type = format.format_type();
        if format_type.is_storyboard() {
            continue;
        }

        let has_video_codec = format.codec_info.video_codec.is_some();
        let has_audio_codec = format.codec_info.audio_codec.is_some();
        let has_video_ext = !matches!(
            format.codec_info.video_ext,
            yt_dlp::model::format::Extension::None | yt_dlp::model::format::Extension::Unknown
        );
        let has_audio_ext = !matches!(
            format.codec_info.audio_ext,
            yt_dlp::model::format::Extension::None | yt_dlp::model::format::Extension::Unknown
        );
        let resolution_audio_only =
            format.video_resolution.resolution.as_deref() == Some("audio only");
        let has_video = has_video_codec || (has_video_ext && !resolution_audio_only);
        let has_audio = has_audio_codec || has_audio_ext;
        let unknown = !has_video_codec
            && !has_audio_codec
            && !has_video_ext
            && !has_audio_ext
            && !resolution_audio_only;
        if !has_video && !has_audio && !unknown {
            continue;
        }

        let http_headers = headers_to_map(&format.download_info.http_headers);
        let protocol = Some(protocol_name(format.protocol).to_string());
        let note = format.format_note.clone();
        let file_size = format
            .file_info
            .filesize
            .or(format.file_info.filesize_approx)
            .and_then(|size| u64::try_from(size).ok());
        let format_id = format.format_id.clone();

        if has_video || unknown {
            let note = manifest_note(format_type, note);
            video_streams.push(VideoStream {
                format_id: format_id.clone(),
                url: url.clone(),
                ext: ext.clone(),
                width: format.video_resolution.width,
                height: format.video_resolution.height,
                fps: format.video_resolution.fps.map(|v| *v),
                video_codec: normalize_codec(format.codec_info.video_codec.clone()),
                total_bitrate: pick_bitrate(
                    format.rates_info.total_rate.map(|v| *v),
                    format.rates_info.video_rate.map(|v| *v),
                ),
                file_size,
                has_audio: has_audio || unknown,
                protocol: protocol.clone(),
                note: note.clone(),
                http_headers: http_headers.clone(),
                cookies: None,
            });
        } else if has_audio {
            let note = manifest_note(format_type, note);
            audio_streams.push(AudioStream {
                format_id,
                url,
                ext,
                audio_codec: normalize_codec(format.codec_info.audio_codec.clone()),
                bitrate: pick_bitrate(
                    format.rates_info.audio_rate.map(|v| *v),
                    format.rates_info.total_rate.map(|v| *v),
                ),
                file_size,
                language: format.language.clone(),
                note,
                protocol,
                http_headers,
                cookies: None,
            });
        }
    }

    video_streams.sort_by_key(|stream| std::cmp::Reverse(stream.rank()));
    audio_streams.sort_by_key(|stream| std::cmp::Reverse(stream.rank()));

    if video_streams.is_empty() && audio_streams.is_empty() {
        return Err(Error::Empty);
    }

    let is_live = video.is_live.unwrap_or(false) || video.live_status == "is_live";
    let title = video.title;
    let extractor = Some(video.extractor_info.extractor_key)
        .filter(|v| !v.is_empty())
        .or_else(|| Some(video.extractor_info.extractor).filter(|v| !v.is_empty()));

    let thumbnails = collect_video_thumbnails(video.thumbnail.as_deref(), &video.thumbnails);
    let thumbnail = thumbnails.first().cloned().or(video.thumbnail);

    Ok(MediaSource {
        id: video.id,
        title: without_live_timestamp(&title).to_string(),
        uploader: video.uploader.or(video.channel),
        duration: video.duration.map(|d| d as f64),
        thumbnail,
        webpage_url: video.webpage_url,
        extractor,
        is_live,
        thumbnails,
        video_streams,
        audio_streams,
    })
}

/// Converts a `yt-dlp` playlist into a resolved playlist document.
pub fn parse_playlist(
    playlist: yt_dlp::model::playlist::Playlist,
    source_url: &str,
) -> ResolvedPlaylist {
    let entries = playlist
        .entries
        .into_iter()
        .filter_map(|entry| {
            let url = normalize_playlist_entry_url(source_url, &entry.url, &entry.id)?;
            let title = if entry.title.trim().is_empty() { entry.id.clone() } else { entry.title };
            let thumbnails = collect_thumbnail_urls(entry.thumbnail.as_deref(), std::iter::empty());
            let thumbnail = thumbnails.first().cloned().or(entry.thumbnail);
            Some(ResolvedPlaylistEntry {
                id: entry.id,
                title,
                url,
                thumbnail,
                thumbnails,
                uploader: entry.uploader,
            })
        })
        .collect();
    ResolvedPlaylist { id: playlist.id, title: playlist.title, url: playlist.url, entries }
}

fn normalize_playlist_entry_url(source_url: &str, raw: &str, id: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return fallback_playlist_entry_url(source_url, id);
    }
    if let Ok(url) = url::Url::parse(raw) {
        return Some(url.to_string());
    }
    if let Ok(base) = url::Url::parse(source_url)
        && let Ok(joined) = base.join(raw)
    {
        return Some(joined.to_string());
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return Some(raw.to_string());
    }
    fallback_playlist_entry_url(source_url, id)
}

fn fallback_playlist_entry_url(source_url: &str, id: &str) -> Option<String> {
    let source = url::Url::parse(source_url).ok()?;
    let host = source.host_str()?.to_ascii_lowercase();
    let is_youtube = host == "youtube.com"
        || host.ends_with(".youtube.com")
        || host == "youtu.be"
        || host.ends_with(".youtu.be")
        || host == "youtube-nocookie.com";
    if is_youtube && !id.trim().is_empty() {
        return Some(format!("https://www.youtube.com/watch?v={id}"));
    }
    None
}

fn pick_bitrate(primary: Option<f64>, fallback: Option<f64>) -> Option<u64> {
    primary.or(fallback).filter(|v| v.is_finite() && *v > 0.0).map(|v| v.round() as u64)
}

fn normalize_codec(codec: Option<String>) -> Option<String> {
    codec.filter(|value| value != "none")
}

fn headers_to_map(
    headers: &yt_dlp::model::format::HttpHeaders,
) -> Option<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    if !headers.user_agent.is_empty() {
        map.insert("User-Agent".to_string(), headers.user_agent.clone());
    }
    if !headers.accept.is_empty() {
        map.insert("Accept".to_string(), headers.accept.clone());
    }
    if !headers.accept_language.is_empty() {
        map.insert("Accept-Language".to_string(), headers.accept_language.clone());
    }
    if !headers.sec_fetch_mode.is_empty() {
        map.insert("Sec-Fetch-Mode".to_string(), headers.sec_fetch_mode.clone());
    }
    for (name, value) in &headers.extra {
        if !name.is_empty() && !value.is_empty() {
            map.insert(name.clone(), value.clone());
        }
    }
    (!map.is_empty()).then_some(map)
}

fn protocol_name(protocol: yt_dlp::model::format::Protocol) -> &'static str {
    match protocol {
        yt_dlp::model::format::Protocol::Https => "https",
        yt_dlp::model::format::Protocol::M3U8Native => "m3u8_native",
        yt_dlp::model::format::Protocol::Mhtml => "mhtml",
        yt_dlp::model::format::Protocol::Unknown => "unknown",
    }
}

fn manifest_note(
    format_type: yt_dlp::model::format::FormatType,
    note: Option<String>,
) -> Option<String> {
    if format_type.is_manifest() {
        Some(note.unwrap_or_else(|| "manifest".to_string()))
    } else {
        note
    }
}

fn collect_video_thumbnails(
    primary: Option<&str>,
    thumbnails: &[yt_dlp::model::thumbnail::Thumbnail],
) -> Vec<String> {
    let mut sorted = thumbnails.iter().collect::<Vec<_>>();
    sorted.sort_by_key(|thumbnail| {
        (thumbnail.preference, thumbnail.width.unwrap_or(0) * thumbnail.height.unwrap_or(0))
    });
    collect_thumbnail_urls(
        primary,
        sorted.into_iter().rev().map(|thumbnail| thumbnail.url.as_str()),
    )
}

fn collect_thumbnail_urls<'a>(
    primary: Option<&'a str>,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let mut urls = Vec::new();
    for url in primary.into_iter().chain(candidates) {
        let trimmed = url.trim();
        if trimmed.is_empty() || urls.iter().any(|existing: &String| existing == trimmed) {
            continue;
        }
        urls.push(trimmed.to_string());
    }
    urls
}

fn without_live_timestamp(title: &str) -> &str {
    let Some((prefix, time)) = title.rsplit_once(' ') else {
        return title;
    };
    let Some((clean_title, date)) = prefix.rsplit_once(' ') else {
        return title;
    };
    let Some((year, month_day)) = date.split_once('-') else {
        return title;
    };
    let Some((month, day)) = month_day.split_once('-') else {
        return title;
    };
    let Some((hour, minute)) = time.split_once(':') else {
        return title;
    };
    let numeric = [year, month, day, hour, minute]
        .iter()
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    if numeric
        && year.len() == 4
        && month.len() == 2
        && day.len() == 2
        && hour.len() == 2
        && minute.len() == 2
    {
        clean_title
    } else {
        title
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: serde_json::Value) -> Result<MediaSource, Error> {
        let video: yt_dlp::model::Video = serde_json::from_value(json).expect("valid video json");
        parse_video(video)
    }

    fn base_video(formats: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "id": "abc123",
            "title": "Live test 2026-11-21 21:45",
            "thumbnail": null,
            "description": null,
            "availability": null,
            "timestamp": null,
            "duration": null,
            "duration_string": null,
            "webpage_url": "https://www.youtube.com/watch?v=abc123",
            "available_at": null,
            "language": null,
            "media_type": null,
            "is_live": true,
            "was_live": null,
            "release_timestamp": null,
            "release_year": null,
            "view_count": null,
            "like_count": null,
            "comment_count": null,
            "channel": "Chan",
            "channel_id": null,
            "channel_url": null,
            "channel_follower_count": null,
            "uploader": null,
            "uploader_id": null,
            "uploader_url": null,
            "channel_is_verified": null,
            "formats": formats,
            "thumbnails": [],
            "automatic_captions": {},
            "subtitles": {},
            "chapters": [],
            "heatmap": null,
            "tags": [],
            "categories": [],
            "age_limit": 0,
            "_has_drm": null,
            "live_status": "is_live",
            "playable_in_embed": true,
            "extractor": "youtube",
            "extractor_key": "Youtube",
            "_version": {"version":"2026.10.26","current_git_head":null,"release_git_head":null,"repository":"yt-dlp/yt-dlp"}
        })
    }

    fn format_json(fields: serde_json::Value) -> serde_json::Value {
        let mut value = serde_json::json!({
            "format": "test",
            "format_id": "0",
            "format_note": null,
            "protocol": "https",
            "language": null,
            "has_drm": null,
            "container": null,
            "available_at": null,
            "language_preference": null,
            "source_preference": null,
            "acodec": null,
            "vcodec": null,
            "audio_ext": "none",
            "video_ext": "none",
            "audio_channels": null,
            "asr": null,
            "width": null,
            "height": null,
            "resolution": null,
            "fps": null,
            "aspect_ratio": null,
            "url": "https://example.com/media",
            "ext": "mp4",
            "http_headers": {"User-Agent":"ua","Accept":"*/*","Accept-Language":"en-US,en","Sec-Fetch-Mode":"navigate"},
            "manifest_url": null,
            "downloader_options": null,
            "quality": null,
            "dynamic_range": null,
            "filesize_approx": null,
            "filesize": null,
            "vbr": null,
            "abr": null,
            "tbr": null,
            "rows": null,
            "columns": null,
            "fragments": null
        });
        let obj = value.as_object_mut().unwrap();
        for (k, v) in fields.as_object().unwrap() {
            obj.insert(k.clone(), v.clone());
        }
        value
    }

    #[test]
    fn live_manifest_formats_are_not_dropped() {
        let source = parse(base_video(serde_json::json!([
            format_json(serde_json::json!({
                "format": "95 - 1280x720",
                "format_id": "95",
                "url": "https://example.com/manifest.m3u8",
                "ext": "mp4",
                "width": 1280,
                "height": 720,
                "fps": 30.0,
                "vcodec": "avc1",
                "acodec": "mp4a.40.2",
                "tbr": 2500.0,
                "protocol": "m3u8_native",
                "resolution": "1280x720",
                "video_ext": "mp4",
                "audio_ext": "none",
                "manifest_url": "https://example.com/manifest.m3u8"
            })),
            format_json(serde_json::json!({
                "format": "140-drc - audio only",
                "format_id": "140-drc",
                "url": "https://example.com/audio.m4a",
                "ext": "m4a",
                "vcodec": "none",
                "acodec": "mp4a.40.2",
                "abr": 129.0,
                "protocol": "https",
                "format_note": "medium",
                "language": "en",
                "resolution": "audio only",
                "video_ext": "none",
                "audio_ext": "m4a"
            }))
        ])))
        .expect("live manifest should parse");

        assert!(source.is_live);
        assert_eq!(source.title, "Live test");
        assert_eq!(source.video_streams.len(), 1);
        assert_eq!(source.video_streams[0].format_id, "95");
        assert_eq!(source.video_streams[0].protocol.as_deref(), Some("m3u8_native"));
        assert_eq!(source.video_streams[0].note.as_deref(), Some("manifest"));
        assert_eq!(source.audio_streams.len(), 1);
    }

    #[test]
    fn storyboard_formats_are_ignored() {
        let err = parse(base_video(serde_json::json!([format_json(serde_json::json!({
            "format": "sb0 - storyboard",
            "format_id": "sb0",
            "url": "https://example.com/story.mhtml",
            "ext": "mhtml",
            "vcodec": "none",
            "acodec": "none",
            "protocol": "https",
            "resolution": "168x94",
            "video_ext": "none",
            "audio_ext": "none"
        }))])))
        .unwrap_err();
        assert!(matches!(err, Error::Empty));
    }

    #[test]
    fn video_thumbnail_candidates_are_collected() {
        let mut video = base_video(serde_json::json!([format_json(serde_json::json!({
            "format": "95 - 1280x720",
            "format_id": "95",
            "url": "https://example.com/manifest.m3u8",
            "ext": "mp4",
            "width": 1280,
            "height": 720,
            "vcodec": "avc1",
            "acodec": "mp4a.40.2",
            "protocol": "m3u8_native"
        }))]));
        video["thumbnail"] = serde_json::json!("https://img/maxresdefault.jpg");
        video["thumbnails"] = serde_json::json!([
            { "id": "small", "url": "https://img/hqdefault.jpg", "preference": 0, "width": 480, "height": 360 },
            { "id": "large", "url": "https://img/maxresdefault.jpg", "preference": 1, "width": 1280, "height": 720 },
            { "id": "medium", "url": "https://img/sddefault.jpg", "preference": 0, "width": 640, "height": 480 }
        ]);
        let source = parse(video).expect("video parses");
        assert_eq!(source.thumbnail.as_deref(), Some("https://img/maxresdefault.jpg"));
        assert_eq!(
            source.thumbnails,
            vec![
                "https://img/maxresdefault.jpg".to_string(),
                "https://img/sddefault.jpg".to_string(),
                "https://img/hqdefault.jpg".to_string()
            ]
        );
    }
}
