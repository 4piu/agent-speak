//! Keyed playback scheduling and device ownership.
//!
//! The actor in this module owns platform audio state on one dedicated thread.
//! MCP handlers only validate and submit jobs; they never hold Rodio or TTS
//! objects themselves. The public MCP policy remains serialized while the
//! internal multi-stream path is developed and validated.

mod actor;
mod audio;
mod tts;

pub use actor::{
    Acceptance, BackendCompletion, Cancellation, CompletionNotifier, ConcurrencyMode,
    EmergencyStop, LifecycleEvent, PLAYBACK_STATUS_RETENTION_ITEMS, PlaybackBackend, PlaybackError,
    PlaybackHandle, PlaybackJob, PlaybackSource, PlaybackState, PlaybackStatus,
};
pub use audio::{
    AudioAdapter, AudioFormat, AudioInfo, OutputDevice, OutputTarget, PreparedAudio, RodioAudio,
    list_output_devices,
};
pub use tts::{
    NativeSystemBackend, SystemBackend, SystemTts, SystemVoice, TtsAdapter, TtsCapabilities,
    list_system_voices,
};
