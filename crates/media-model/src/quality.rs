//! How the user asked for a stream to be selected.

/// A stream choice offered to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Quality {
    /// The highest quality the extractor offers.
    Best,
    /// An explicit pair of stream identifiers.
    ///
    /// Either part may be absent: a video-only media has no audio, an
    /// audio-only media has no video, and a combined stream is named in
    /// `video` with `audio` empty.
    Streams {
        /// Identifier of the video (or combined) stream.
        video: Option<String>,
        /// Identifier of the audio stream, when the video stream has none.
        audio: Option<String>,
    },
}
