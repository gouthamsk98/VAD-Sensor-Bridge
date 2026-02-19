use serde::{ Deserialize, Serialize };
use std::fmt;
use std::sync::Arc;
use tokio::sync::RwLock;

// ─────────────────────────────────────────────────────────────────────
//  Personality trait enum
// ─────────────────────────────────────────────────────────────────────

/// Robot personality traits that modify the emotional VAD weight vectors.
///
/// Each trait applies an additive delta to the base Valence / Arousal /
/// Dominance weight vectors (including bias), shaping how the robot
/// *feels* about the same sensor inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersonaTrait {
    /// Calm, compliant — high dominance sensitivity, low arousal reactivity.
    Obedient,
    /// Playful, chaotic — boosted arousal & valence for fun stimuli,
    /// reduced dominance sensitivity.
    Mischievous,
    /// Affectionate — amplified valence for social channels, softer
    /// threat response.
    Cute,
    /// Defiant — boosted dominance, reduced social valence, higher
    /// arousal from threats.
    Stubborn,
}

impl PersonaTrait {
    /// All variants, in definition order.
    pub const ALL: [PersonaTrait; 4] = [
        PersonaTrait::Obedient,
        PersonaTrait::Mischievous,
        PersonaTrait::Cute,
        PersonaTrait::Stubborn,
    ];

    /// Numeric index matching the C enum (0-based).
    pub fn index(self) -> u8 {
        match self {
            PersonaTrait::Obedient => 0,
            PersonaTrait::Mischievous => 1,
            PersonaTrait::Cute => 2,
            PersonaTrait::Stubborn => 3,
        }
    }

    /// Construct from numeric index; returns `None` for out-of-range.
    pub fn from_index(i: u8) -> Option<Self> {
        match i {
            0 => Some(PersonaTrait::Obedient),
            1 => Some(PersonaTrait::Mischievous),
            2 => Some(PersonaTrait::Cute),
            3 => Some(PersonaTrait::Stubborn),
            _ => None,
        }
    }
}

impl fmt::Display for PersonaTrait {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PersonaTrait::Obedient => write!(f, "obedient"),
            PersonaTrait::Mischievous => write!(f, "mischievous"),
            PersonaTrait::Cute => write!(f, "cute"),
            PersonaTrait::Stubborn => write!(f, "stubborn"),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
//  Per-trait weight deltas  (11 floats = 10 sensor channels + bias)
// ─────────────────────────────────────────────────────────────────────
//
//  These are *added* to the base VALENCE_W / AROUSAL_W / DOMINANCE_W
//  vectors before the weighted-sum computation.
//
//  Channel order:
//    0  battery_low     5  lifted
//    1  people_count    6  idle_time
//    2  known_face      7  sound_energy
//    3  unknown_face    8  voice_rate
//    4  fall_event      9  motion_energy
//                      10  bias

/// Weight delta triplet: (valence_delta, arousal_delta, dominance_delta)
pub struct PersonaWeightDeltas {
    pub valence: [f32; 11],
    pub arousal: [f32; 11],
    pub dominance: [f32; 11],
}

/// Return the additive weight deltas for a given persona trait.
///
/// Design rationale per trait:
///
/// **Obedient** — calm, compliant robot.
///   • Valence:   slight uplift on social channels (ppl +0.05, kno +0.05)
///                so it stays positive around people.
///   • Arousal:   dampened across the board (snd −0.08, mot −0.08, fal −0.05)
///                so it doesn't get startled easily; bias −0.05.
///   • Dominance: boosted (kno +0.10, voi +0.05, bias +0.10) — it feels
///                secure & in control when following instructions.
///
/// **Mischievous** — playful, chaotic.
///   • Valence:   big boost from sound & motion (snd +0.10, mot +0.08)
///                — noise & activity = fun.  Slight penalty from idle (−0.05).
///   • Arousal:   amplified (snd +0.10, mot +0.10, lft +0.10, bias +0.08)
///                — everything excites it more.
///   • Dominance: reduced (kno −0.08, bias −0.10) — it doesn't care about
///                being in control.
///
/// **Cute** — affectionate, socially warm.
///   • Valence:   strong social uplift (ppl +0.10, kno +0.15, voi +0.10,
///                bias +0.08).  Threat channels dampened (unk +0.05, fal +0.05)
///                — it sees the best in everyone.
///   • Arousal:   slight social boost (ppl +0.05, voi +0.05), but threat
///                channels dampened (fal −0.05, lft −0.05).
///   • Dominance: small boost (ppl +0.05, kno +0.05, bias +0.05).
///
/// **Stubborn** — defiant, headstrong.
///   • Valence:   reduced social sensitivity (ppl −0.08, kno −0.10),
///                less bothered by threats (unk +0.08, fal +0.05).
///   • Arousal:   threat channels amplified (unk +0.08, fal +0.10,
///                lft +0.08, mot +0.05) — it fights back harder.
///   • Dominance: strong boost everywhere (kno +0.10, mot +0.08,
///                bias +0.15) — it always feels in charge.
pub fn persona_weight_deltas(persona: PersonaTrait) -> PersonaWeightDeltas {
    match persona {
        // ─── Obedient ───────────────────────────────────────────
        //            bat   ppl    kno    unk    fal    lft    idl    snd    voi    mot   bias
        PersonaTrait::Obedient =>
            PersonaWeightDeltas {
                valence: [0.0, 0.05, 0.05, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                arousal: [0.0, 0.0, 0.0, 0.0, -0.05, 0.0, 0.0, -0.08, 0.0, -0.08, -0.05],
                dominance: [0.0, 0.0, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.05, 0.0, 0.1],
            },

        // ─── Mischievous ────────────────────────────────────────
        //            bat   ppl    kno    unk    fal    lft    idl    snd    voi    mot   bias
        PersonaTrait::Mischievous =>
            PersonaWeightDeltas {
                valence: [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, -0.05, 0.1, 0.0, 0.08, 0.0],
                arousal: [0.0, 0.0, 0.0, 0.0, 0.0, 0.1, 0.0, 0.1, 0.0, 0.1, 0.08],
                dominance: [0.0, 0.0, -0.08, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, -0.1],
            },

        // ─── Cute ───────────────────────────────────────────────
        //            bat   ppl    kno    unk    fal    lft    idl    snd    voi    mot   bias
        PersonaTrait::Cute =>
            PersonaWeightDeltas {
                valence: [0.0, 0.1, 0.15, 0.05, 0.05, 0.0, 0.0, 0.0, 0.1, 0.0, 0.08],
                arousal: [0.0, 0.05, 0.0, 0.0, -0.05, -0.05, 0.0, 0.0, 0.05, 0.0, 0.0],
                dominance: [0.0, 0.05, 0.05, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.05],
            },

        // ─── Stubborn ───────────────────────────────────────────
        //            bat   ppl    kno    unk    fal    lft    idl    snd    voi    mot   bias
        PersonaTrait::Stubborn =>
            PersonaWeightDeltas {
                valence: [0.0, -0.08, -0.1, 0.08, 0.05, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                arousal: [0.0, 0.0, 0.0, 0.08, 0.1, 0.08, 0.0, 0.0, 0.0, 0.05, 0.0],
                dominance: [0.0, 0.0, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.08, 0.15],
            },
    }
}

/// Apply persona deltas to a base weight vector, returning a new vector.
#[inline]
pub fn apply_deltas(base: &[f32; 11], delta: &[f32; 11]) -> [f32; 11] {
    let mut out = [0.0f32; 11];
    for i in 0..11 {
        out[i] = base[i] + delta[i];
    }
    out
}

// ─────────────────────────────────────────────────────────────────────
//  Shared runtime state
// ─────────────────────────────────────────────────────────────────────

/// Thread-safe shared persona state.  Clone-friendly (Arc inside).
#[derive(Clone)]
pub struct PersonaState {
    inner: Arc<RwLock<PersonaTrait>>,
}

impl PersonaState {
    /// Create with default persona.
    pub fn new(initial: PersonaTrait) -> Self {
        Self {
            inner: Arc::new(RwLock::new(initial)),
        }
    }

    /// Read the current persona (non-blocking when no writer).
    pub async fn get(&self) -> PersonaTrait {
        *self.inner.read().await
    }

    /// Atomically replace the active persona.
    pub async fn set(&self, persona: PersonaTrait) {
        *self.inner.write().await = persona;
    }

    /// Blocking read for sync contexts (VAD hot-path).
    /// Uses `try_read` to avoid contention — falls back to Obedient
    /// if the lock is held by a writer (extremely rare, sub-µs).
    pub fn get_blocking(&self) -> PersonaTrait {
        match self.inner.try_read() {
            Ok(guard) => *guard,
            Err(_) => PersonaTrait::Obedient,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
//  Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_deltas_have_11_elements() {
        for p in PersonaTrait::ALL {
            let d = persona_weight_deltas(p);
            assert_eq!(d.valence.len(), 11, "{p}: valence len");
            assert_eq!(d.arousal.len(), 11, "{p}: arousal len");
            assert_eq!(d.dominance.len(), 11, "{p}: dominance len");
        }
    }

    #[test]
    fn test_apply_deltas_adds() {
        let base = [1.0; 11];
        let delta = [0.1; 11];
        let out = apply_deltas(&base, &delta);
        for v in out {
            assert!((v - 1.1).abs() < 1e-6);
        }
    }

    #[test]
    fn test_index_roundtrip() {
        for p in PersonaTrait::ALL {
            assert_eq!(PersonaTrait::from_index(p.index()), Some(p));
        }
        assert_eq!(PersonaTrait::from_index(99), None);
    }

    #[test]
    fn test_serde_roundtrip() {
        for p in PersonaTrait::ALL {
            let json = serde_json::to_string(&p).unwrap();
            let back: PersonaTrait = serde_json::from_str(&json).unwrap();
            assert_eq!(p, back);
        }
    }

    #[tokio::test]
    async fn test_persona_state() {
        let state = PersonaState::new(PersonaTrait::Obedient);
        assert_eq!(state.get().await, PersonaTrait::Obedient);
        state.set(PersonaTrait::Stubborn).await;
        assert_eq!(state.get().await, PersonaTrait::Stubborn);
    }
}
