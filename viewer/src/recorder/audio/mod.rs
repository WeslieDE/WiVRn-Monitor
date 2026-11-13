//! Audio recording: capture (microphone and application sound), mixing, AAC encoding. See docs/TECHNICAL.md.
//!
//! Data flow: capture threads (one per source, blocking reads) -> per-source buffers -> engine thread (clock driven:
//! mixes 10 ms blocks of the recording timeline, encodes AAC-LC) -> access units over a channel -> encoder/muxer
//! thread of the recorder. None of this runs on the UI or network thread.

pub mod capture;
pub mod device;
pub mod engine;
pub mod mix;
pub mod pace;

/// Sample rate of the whole audio path.
pub const RATE: u32 = 48_000;

/// What the user asked for (applies to the next recording).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioOptions {
    /// Main switch.
    pub enabled: bool,
    pub mic: bool,
    pub app: bool,
}

impl Default for AudioOptions {
    fn default() -> Self {
        AudioOptions { enabled: true, mic: true, app: true }
    }
}

impl AudioOptions {
    /// True if at least one source is switched on.
    pub fn active(&self) -> bool {
        self.enabled && (self.mic || self.app)
    }
}

/// The capture devices to use (each `None` = not recorded).
pub struct Sources {
    pub mic: Option<capture::Opener>,
    pub app: Option<capture::Opener>,
}

/// Produces the sources; runs on the audio thread because it may load libraries and run `pactl`.
pub type SourceFactory = Box<dyn FnOnce() -> Result<Sources, String> + Send>;

/// Factory for the real devices.
pub fn pulse_factory(opts: AudioOptions) -> SourceFactory {
    Box::new(move || device::pulse_sources(opts))
}
