use std::sync::Arc;

use fastembed::TextEmbedding;
use tokio::sync::Mutex;

use crate::error::FrameworkError;

pub(super) async fn embed_text(
    embedder: &Arc<Mutex<TextEmbedding>>,
    text: &str,
) -> Result<Vec<f32>, FrameworkError> {
    let mut model = embedder.lock().await;
    let embeddings = model
        .embed(vec![text.to_owned()], None)
        .map_err(|e| FrameworkError::Config(format!("embedding failed: {e}")))?;
    embeddings
        .into_iter()
        .next()
        .ok_or_else(|| FrameworkError::Config("embedder returned no vector".to_owned()))
}

pub(super) fn encode_f32_blob(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for value in values {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}
