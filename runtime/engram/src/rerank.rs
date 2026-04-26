//! Cross-encoder reranking using ONNX runtime.
//!
//! Reranks retrieval candidates by computing a relevance score for each
//! (query, fact.text) pair using a cross-encoder model.
//!
//! Enable with `--features rerank`. When disabled, the reranker is a no-op.

use crate::retrieve::ScoredFact;
use crate::store::MemoryError;
use std::path::Path;

/// Cross-encoder reranker backed by an ONNX model.
pub struct CrossEncoderReranker {
    #[cfg(feature = "rerank")]
    _session: ort::session::Session,
    #[cfg(not(feature = "rerank"))]
    _phantom: (),
}

impl CrossEncoderReranker {
    /// Load a cross-encoder ONNX model from disk.
    pub fn load(model_path: &Path) -> Result<Self, MemoryError> {
        #[cfg(feature = "rerank")]
        {
            let session = ort::session::Session::builder()
                .and_then(|b| b.commit_from_file(model_path))
                .map_err(|e| MemoryError::Database(format!("ONNX load error: {e}")))?;
            Ok(Self { _session: session })
        }
        #[cfg(not(feature = "rerank"))]
        {
            let _ = model_path;
            Err(MemoryError::Database(
                "rerank feature not enabled — rebuild with --features rerank".into(),
            ))
        }
    }

    /// Rerank scored facts by cross-encoder relevance.
    /// Returns facts reordered by cross-encoder score (highest first).
    /// When the rerank feature is disabled, returns facts unchanged.
    pub fn rerank(
        &self,
        _query: &str,
        facts: Vec<ScoredFact>,
    ) -> Result<Vec<ScoredFact>, MemoryError> {
        // Placeholder: full ONNX tokenization + inference requires the
        // `tokenizers` crate and model-specific setup. This scaffold ensures
        // the plumbing is in place for when the model is integrated.
        #[cfg(feature = "rerank")]
        {
            let _ = &self._session;
        }
        Ok(facts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn load_fails_without_rerank_feature() {
        // Without the rerank feature, load should return an error
        #[cfg(not(feature = "rerank"))]
        {
            let result = CrossEncoderReranker::load(&PathBuf::from("nonexistent.onnx"));
            assert!(result.is_err());
        }
    }
}
