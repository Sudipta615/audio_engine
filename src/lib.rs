/// `audio-output` is a REQUIRED feature, despite being listed under
/// `[features]`.
///
/// It is not genuinely optional: `output::output` (the `Output` trait and its
/// factory), `output::capabilities`, and every per-OS backend reach `cpal`
/// unconditionally, so the crate cannot be built without it. The alternative
/// would be to thread `#[cfg(feature = "audio-output")]` through the whole
/// output layer, which would mean either stubbing `Output` into a
/// backend-less no-op or duplicating every call site — both far more
/// expensive than the one optional backend this crate ships.
///
/// The feature is kept in `[features]` because it is the single switch for
/// "build the cpal backend", and because `wasapi-native`, `pipewire`, `jack`
/// and `asio-native` all compose through it.
///
/// Without this guard a `--no-default-features` build produced a dozen
/// unrelated `unresolved import cpal` errors pointing at backend internals
/// rather than at the feature that actually had to be on.
#[cfg(not(feature = "audio-output"))]
compile_error!(
    "the `audio-output` feature is required: the engine's output layer is built \
     on cpal and cannot be compiled without it. Drop `--no-default-features` or \
     pass `--features audio-output`."
);

pub mod audio_io;
pub mod buffer;
pub mod commands;
pub mod decode;
pub mod diagnostics;
pub mod dsp;
pub mod dsp_utils;
// NOT gated on `audio-output`. The engine state machine is the crate's core
// and compiles without a backend; gating the whole module made
// `--no-default-features` fail with ten unrelated "cannot find module
// `engine`" errors from `commands`, `source`, `diagnostics` and the prelude
// re-exports, all of which reference `crate::engine::*` unconditionally.
pub mod engine;
pub mod eval;
pub mod events;
#[cfg(feature = "c-ffi")]
pub mod ffi;
pub mod fx;
pub mod governance;
pub mod network_audio;
pub mod output;
pub mod paths;
pub mod playback_info;
pub mod playlist;
pub mod profile;
pub mod sink;
pub mod source;
pub mod spatial;
pub mod standards;
pub mod state;
pub mod track_cache;

// Re-exports for convenience
pub use commands::EngineCommand;
pub use diagnostics::{BitPerfectCause, Diagnostic, DiagnosticKind};
pub use dsp::{ProfessionalMeterSnapshot, ProfessionalMeters};
pub use events::EngineEvent;
pub use fx::{
    Chorus, CombFilter, DistortionType, Flanger, Phaser, PingPongDelay, RingModulator, Saturator,
};
pub use playback_info::{PlaybackInfo, PlaybackState, SpatialTelemetry};
pub use playlist::{Playlist, RepeatMode};
pub use source::AudioSource;
pub use track_cache::{CachedTrackInfo, TrackCache};

pub use engine::cue_split::{CueSegmentInfo, PregapPolicy};
pub use engine::dsp_persistence::DspStateStore;
#[cfg(feature = "audio-output")]
pub use engine::{AudioEngine, EngineError, EngineHandle, EngineWake};
pub use engine::{OfflineRenderResult, OfflineRenderer};

#[cfg(feature = "network-streaming")]
pub use audio_io::NetworkByteSource;
pub use audio_io::{AudioByteSource, FileByteSource, MemoryByteSource};
pub use config::{AudioBackend, EngineConfig, ResamplerQuality, SampleRatePolicy};
pub use decode::extract_track_metadata;
pub use decode::{SharedPcm, SharedPcmDecoder};
pub use profile::{AnalysisMask, AudioProfile, DynamicCharacter, ProfileError};
pub use sink::{DacSink, NoopSink, SampleSink, VecSink};

pub mod prelude {
    pub use crate::engine::dsp_persistence::DspStateStore;
    #[cfg(feature = "audio-output")]
    pub use crate::engine::{AudioEngine, EngineError, EngineHandle, EngineWake, PlaybackStream};
    pub use crate::engine::{OfflineRenderResult, OfflineRenderer};
    pub use crate::{
        buffer::{
            validate_audio_block, AudioBlockError, AudioChunk, AudioFrame, BufferError,
            FixedFrameBuffer, DEFAULT_SAMPLE_RATE, MAX_AUDIO_BLOCK_FRAMES,
        },
        commands::EngineCommand,
        decode::extract_track_metadata,
        dsp::aelog::{
            content_address, graph_fingerprint, log_hash, render_cached, replay_events,
            replay_render, Aelog, AelogCache, AelogError, AelogRecorder, RecordedCommand,
            ReplayError, ReplayOutcome, SessionHeader, AELOG_VERSION,
        },
        dsp::graph2::prod::{
            AutomationPoint, AutomationTarget, DspGraph, DspNode, DuckState, Graph2ControlHandle,
            Graph2Engine, GraphControlHandle, GraphGeneration, PanLaw, MAX_DUCK_TARGETS,
            MAX_MIX_SLOTS,
        },
        dsp::graph2::{
            analyze, compensate, node_latency, rt, ExecutionOrder, Graph2, Graph2Error, HrtfSource,
            LatencyReport, NodeCapabilities, NodeDef, NodeId, NodeKind, NodeParams,
            OfflineExecutor, PortId, PortSpec, ProdStage, RtExecutor, RtPlan, RtPlanError,
            RtScenes, SignalType, SourceParams, TestSignal, ValidationReport,
            RESAMPLER_DEFAULT_QUALITY,
        },
        dsp::pipeline::{DspPipeline, OutputSampleFormat},
        dsp::timeline::{
            AudioClock, CurveBeats, EventError, EventId, EventPayload, EventTime, Quantize,
            ScheduledEvent, TempoMap, TempoPoint, TempoRamp, Timeline, TimelineRegion,
            TransportState,
        },
        events::EngineEvent,
        playback_info::{PlaybackInfo, PlaybackState},
        playlist::{Playlist, RepeatMode},
        profile::{
            analyze_decoder, analyze_path, analyze_path_cached, lookup, lookup_for_id, store,
            store_with_id, AnalysisMask, AudioProfile, ContentProfile, DynamicCharacter,
            DynamicProfile, LoudnessProfile, MaskingProfile, ProfileAnalyzer, ProfileError,
            SpatialProfile, SpectralProfile, StereoProfile, TransientProfile,
            AUDIO_PROFILE_VERSION,
        },
        sink::{DacSink, NoopSink, SampleSink, VecSink},
        source::AudioSource,
        spatial::{
            encode_plane_wave, head_shadow_alpha, rotate_bus_frame, sh_foa, spectral_taps,
            woodworth_itd_sec, AcousticBaker, AcousticPath, AcousticRoom, AcousticTransmission,
            AcousticWorld, AirAbsorption, AirRolloffModel, AmbisonicDecoder, AmbisonicRenderer,
            BakePolicy, BakedObject, BakedPath, BakedScene, BasicPanner, BedId, BinauralRenderer,
            CustomDirectivity, DecoderPolicy, DiffractionEdge, Directivity, DistanceModel, Ear,
            FieldId, HeadSample, HeadShadow, HeadTracker, HybridBlockInputs, LayoutCalibration,
            Listener, ListenerPose, MaterialKind, MaterialSpectrum, ObjectAudioRef, ObjectId,
            Occlusion, PathKind, Portal, Quat, RenderError, RendererKind, Room, SpatialAudioObject,
            SpatialBed, SpatialBedStore, SpatialField, SpatialFieldStore, SpatialHealthSnapshot,
            SpatialObjectStore, SpatialRenderer, SpatialScene, Speaker, SpeakerId, SpeakerLayout,
            TrackingConfig, VbapRenderer, Vec3, Wall, ACOUSTIC_IR_LEN,
        },
    };
    pub use config::{AudioBackend, ChannelPolicy, ResamplerQuality};
}
