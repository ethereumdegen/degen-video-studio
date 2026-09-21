//! The audio engine: sample-exact mixing, ducking, loudness and silence.
//!
//! Video editing tolerates a frame of slop; audio does not. A clip that lands one sample
//! late clicks, and a timeline built out of floating-point second offsets accumulates that
//! error until lip sync visibly drifts. So every position here is derived from the exact
//! rational time in the document — `Time::sample_round` — and never from a running float
//! cursor. A clip that starts at `1/3` s on a 48 kHz timeline starts at sample 16000, in
//! every span it is mixed into, no matter where the span begins.
//!
//! The second reason this crate exists is that an agent cannot hear the result. Mixing is
//! additive with no limiter and no automatic make-up gain: nothing here quietly rescues a
//! clipped mix. Instead [`analyze_loudness`] reports integrated LUFS, true peak, loudness
//! range and the number of samples that reached full scale, so "is it too loud" is a number
//! an agent can compare against a target rather than a thing it must listen for.

pub mod loudness;
pub mod mix;
pub mod ops;
pub mod silence;

pub use loudness::{analyze_loudness, normalize_gain_db, Loudness};
pub use mix::{clip_carries_audio, has_audio, mix_span, mix_tracks, MixSpec};
pub use silence::{detect_silence, detect_speech};

/// Add this crate's ops (`audio.*` plus `seq.trim-silence`) to a registry.
pub fn register(registry: &mut dvs_core::op::Registry) {
    ops::register(registry);
}

/// Decibels to a linear amplitude multiplier.
///
/// Shared by the mixer, the ops and the analysers so that "−6 dB" means one number
/// everywhere; a second spelling of this conversion would be a second rounding of every
/// gain in the project.
pub fn db_to_gain(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

/// Linear amplitude to decibels. Silence maps to [`SILENCE_FLOOR_DB`] rather than `-inf`,
/// because the value ends up in JSON and an agent compares it numerically.
pub fn gain_to_db(gain: f32) -> f32 {
    if gain <= 0.0 {
        return SILENCE_FLOOR_DB as f32;
    }
    (20.0 * f64::from(gain).log10()).max(SILENCE_FLOOR_DB) as f32
}

/// The level reported instead of negative infinity.
///
/// `-inf` serializes to `null` in JSON, which forces every consumer to special-case it and
/// makes `measured < target` silently false. A floor 100 dB below full scale is far below
/// anything audible and always compares the way a reader expects.
pub const SILENCE_FLOOR_DB: f64 = -100.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_and_gain_round_trip() {
        for db in [-24.0f32, -6.0, -0.5, 0.0, 3.0, 12.0] {
            assert!(
                (gain_to_db(db_to_gain(db)) - db).abs() < 1e-3,
                "{db} dB did not survive the round trip"
            );
        }
    }

    #[test]
    fn minus_six_db_halves_amplitude() {
        assert!((db_to_gain(-6.0) - 0.5).abs() < 0.01, "−6 dB must halve amplitude");
    }

    #[test]
    fn silence_reports_the_floor_not_negative_infinity() {
        let db = gain_to_db(0.0);
        assert!(db.is_finite(), "silence must report a finite level for JSON");
        assert_eq!(db, SILENCE_FLOOR_DB as f32);
    }
}
