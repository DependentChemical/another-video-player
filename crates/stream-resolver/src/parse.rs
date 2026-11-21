//! Turning `yt-dlp` crate metadata into a [`MediaSource`].

use std::collections::BTreeMap;

use media_model::{AudioStream, MediaSource, VideoStream};

use crate::Error;

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
        let has_video = format_type.is_video() || format_type.is_audio_and_video();
        let has_audio = format_type.is_audio() || format_type.is_audio_and_video();
        if !has_video && !has_audio {
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

        if has_video {
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
                has_audio,
                protocol: protocol.clone(),
                note: note.clone(),
                http_headers: http_headers.clone(),
                cookies: None,
            });
        } else if has_audio {
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

    Ok(MediaSource {
        id: video.id,
        title: without_live_timestamp(&title).to_string(),
        uploader: video.uploader.or(video.channel),
        duration: video.duration.map(|d| d as f64),
        thumbnail: video.thumbnail,
        webpage_url: video.webpage_url,
        extractor,
        is_live,
        video_streams,
        audio_streams,
    })
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
