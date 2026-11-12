//! How the user asked for a stream to be selected.

/// A quality choice offered to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Quality {
    /// The highest quality the extractor offers.
    Best,
    /// A specific vertical resolution, e.g. `1080`.
    Height(u32),
    /// An explicit pair of stream identifiers.
    Streams {
        /// Identifier of the video stream.
        video: String,
        /// Identifier of the audio stream, when the video stream has none.
        audio: Option<String>,
    },
}
