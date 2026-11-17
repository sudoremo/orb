//! Turning messages and queries into vectors.
//!
//! The model is a static embedder: each token has a fixed vector and a text's
//! embedding is their mean. That makes it useless for anything needing context,
//! and extremely fast for what it is used for here — 69,000 messages a second
//! on this machine, no GPU, no batching tricks. Embedding an entire 290,000
//! message mailbox takes about four seconds.
//!
//! That speed shapes the whole design. Vectors are stored binary-quantised, one
//! bit per dimension, because at 290,000 messages that is 16MB against 305MB
//! for full precision and a 3.5ms search against 24ms. Quantising that hard
//! costs ranking quality — but since re-embedding is nearly free, the quality
//! comes back by re-embedding the handful of candidates the index returns and
//! ranking those exactly. The index only has to be roughly right; the answer is
//! exact.
//!
//! The model ships inside the app bundle, so nothing is downloaded and search
//! works offline like the rest of Orb.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::{Error, Result};

/// Dimensions produced by the model. Fixed by the model, checked on load.
pub const DIMENSIONS: usize = 256;

/// Bytes per stored vector, one bit per dimension.
pub const PACKED_BYTES: usize = DIMENSIONS / 8;

/// How much of a message is embedded.
///
/// A static model averages token vectors, so past a point more text only pulls
/// the result towards the average of the language and away from what the
/// message is about. The first couple of thousand characters carry the subject,
/// the greeting and the substance; the rest is usually quoted history and
/// signatures, which every message has and which therefore distinguishes none.
const MAX_EMBEDDED_CHARS: usize = 2000;

/// Environment variable naming the model directory.
///
/// How the packaged app points at the copy inside its own bundle, without the
/// core needing to know anything about app bundles.
pub const MODEL_PATH_ENV: &str = "ORB_EMBEDDING_MODEL";

/// The text that stands for a message, for both indexing and re-ranking.
///
/// One function so the two can never disagree: a message re-ranked against text
/// assembled differently from the text it was indexed with would be scored
/// against a vector it does not have.
pub fn embedding_text(subject: Option<&str>, body: Option<&str>) -> String {
    let subject = subject.unwrap_or("").trim();
    let body = body.unwrap_or("").trim();

    // The subject is repeated deliberately. It is one line against a thousand,
    // and in a mean-pooled embedding a single line otherwise contributes almost
    // nothing — yet it is the best one-line summary a message has.
    let mut text = String::with_capacity(MAX_EMBEDDED_CHARS + subject.len() * 2);
    if !subject.is_empty() {
        text.push_str(subject);
        text.push_str(". ");
        text.push_str(subject);
        text.push_str(". ");
    }

    // Truncated on a character boundary, not a byte one.
    let remaining = MAX_EMBEDDED_CHARS.saturating_sub(text.chars().count());
    text.extend(body.chars().take(remaining));
    text
}

/// Pack a full-precision vector into one bit per dimension.
///
/// The sign of each dimension, which is what survives quantisation and what
/// Hamming distance then compares. Anything at or below zero is a zero bit.
pub fn pack(vector: &[f32]) -> Vec<u8> {
    let mut packed = vec![0u8; vector.len().div_ceil(8)];
    for (index, value) in vector.iter().enumerate() {
        if *value > 0.0 {
            packed[index / 8] |= 1 << (index % 8);
        }
    }
    packed
}

/// Cosine similarity, for re-ranking candidates at full precision.
///
/// Returns a value in -1..=1, higher being closer. Two vectors of different
/// lengths, or either being all zeros, score 0 rather than dividing by zero.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    let magnitude = norm_a.sqrt() * norm_b.sqrt();
    if magnitude == 0.0 {
        0.0
    } else {
        dot / magnitude
    }
}

/// A loaded embedding model.
pub struct Embedder {
    model: model2vec_rs::model::StaticModel,
}

impl std::fmt::Debug for Embedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Embedder").finish_non_exhaustive()
    }
}

impl Embedder {
    /// Load a model from a directory holding `model.safetensors` and
    /// `tokenizer.json`.
    pub fn load(directory: &Path) -> Result<Self> {
        if !directory.join("model.safetensors").is_file() {
            return Err(Error::NotFound(format!(
                "no embedding model at {} (expected model.safetensors)",
                directory.display()
            )));
        }

        let path = directory.to_string_lossy().to_string();
        let model = model2vec_rs::model::StaticModel::from_pretrained(&path, None, None, None)
            .map_err(|e| Error::NotFound(format!("could not load embedding model: {e}")))?;

        let probe = model.encode(&["dimension probe".to_string()]);
        let dimensions = probe.first().map(|v| v.len()).unwrap_or(0);
        if dimensions != DIMENSIONS {
            return Err(Error::NotFound(format!(
                "embedding model produces {dimensions} dimensions, expected {DIMENSIONS}; \
                 the stored index would not match it"
            )));
        }

        Ok(Self { model })
    }

    /// Embed several texts at once.
    pub fn encode(&self, texts: &[String]) -> Vec<Vec<f32>> {
        if texts.is_empty() {
            return Vec::new();
        }
        self.model.encode(texts)
    }

    /// Embed one text.
    pub fn encode_one(&self, text: &str) -> Vec<f32> {
        self.encode(std::slice::from_ref(&text.to_string()))
            .pop()
            .unwrap_or_default()
    }
}

/// Where the model lives, if it is available at all.
///
/// Checked in order: the path configured explicitly, then the one the packaged
/// app exports. Absent from both means semantic search is simply off — exact
/// search does not depend on any of this and keeps working.
pub fn model_directory(configured: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = configured.filter(|p| !p.trim().is_empty()) {
        let expanded = shellexpand::tilde(path).to_string();
        return Some(PathBuf::from(expanded));
    }
    std::env::var(MODEL_PATH_ENV)
        .ok()
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
}

/// Process-wide model, loaded once on first use.
///
/// Loading reads 30MB from disk and builds a tokenizer, which is worth doing at
/// most once, and only if a semantic search is actually run — a session that
/// never uses one should not pay for it.
static MODEL: OnceLock<Option<Arc<Embedder>>> = OnceLock::new();

/// The shared model, loading it if this is the first call.
///
/// `None` means no model is configured or it failed to load; the caller reports
/// that as semantic search being unavailable rather than as a broken search.
pub fn shared(configured: Option<&str>) -> Option<Arc<Embedder>> {
    MODEL
        .get_or_init(|| {
            let directory = model_directory(configured)?;
            match Embedder::load(&directory) {
                Ok(embedder) => Some(Arc::new(embedder)),
                Err(error) => {
                    crate::logger::warn(&format!(
                        "semantic search unavailable: {error}. Exact search is unaffected."
                    ));
                    None
                }
            }
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packing_keeps_one_bit_per_dimension() {
        let vector = vec![0.5, -0.5, 0.1, -0.1, 0.0, 2.0, -3.0, 0.7];
        let packed = pack(&vector);
        assert_eq!(packed.len(), 1);
        // Bits are set for positive dimensions, least significant first:
        // 0, 2, 5, 7 -> 1010_0101.
        assert_eq!(packed[0], 0b1010_0101);
    }

    #[test]
    fn packing_rounds_up_to_whole_bytes() {
        assert_eq!(pack(&[1.0]).len(), 1);
        assert_eq!(pack(&vec![1.0; 256]).len(), PACKED_BYTES);
        assert_eq!(pack(&[]).len(), 0);
    }

    #[test]
    fn zero_is_not_a_set_bit() {
        // Exactly zero carries no direction, so it must not read as positive.
        assert_eq!(pack(&[0.0; 8])[0], 0);
    }

    #[test]
    fn cosine_ranks_by_direction_not_magnitude() {
        let a = vec![1.0, 0.0, 0.0];
        assert!((cosine(&a, &[2.0, 0.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&a, &[0.0, 1.0, 0.0]).abs() < 1e-6);
        assert!((cosine(&a, &[-1.0, 0.0, 0.0]) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_is_defined_for_degenerate_input() {
        // A zero vector has no direction; scoring it must not divide by zero.
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        // Mismatched lengths mean the index and the model disagree, which is a
        // bug elsewhere; scoring 0 keeps it from being ranked first.
        assert_eq!(cosine(&[1.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn embedding_text_leads_with_the_subject_twice() {
        let text = embedding_text(Some("Invoice overdue"), Some("Please pay."));
        assert!(text.starts_with("Invoice overdue. Invoice overdue. "));
        assert!(text.ends_with("Please pay."));
    }

    #[test]
    fn embedding_text_survives_missing_parts() {
        assert_eq!(embedding_text(None, None), "");
        assert_eq!(embedding_text(None, Some("body only")), "body only");
        assert_eq!(
            embedding_text(Some("subject only"), None),
            "subject only. subject only. "
        );
    }

    #[test]
    fn embedding_text_is_bounded() {
        let long = "x".repeat(50_000);
        let text = embedding_text(Some("Subject"), Some(&long));
        assert!(text.chars().count() <= MAX_EMBEDDED_CHARS + 32);
    }

    #[test]
    fn embedding_text_truncates_on_character_boundaries() {
        // Byte truncation would split a multi-byte character and panic or
        // produce invalid text.
        let body = "ä".repeat(50_000);
        let text = embedding_text(Some("Grüezi"), Some(&body));
        assert!(text.chars().count() <= MAX_EMBEDDED_CHARS + 32);
    }

    #[test]
    fn an_unset_model_path_means_disabled_rather_than_broken() {
        assert_eq!(model_directory(Some("   ")), model_directory(None));
        assert_eq!(
            model_directory(Some("~/models/potion")),
            Some(PathBuf::from(
                shellexpand::tilde("~/models/potion").to_string()
            ))
        );
    }
}
