use crate::{
    domain::{Bubble, Rect},
    error::{FukidashiError, Result},
};

#[derive(Debug, Clone, Copy)]
pub struct Detection {
    pub label: i64,
    pub bbox: Rect,
    pub score: f32,
}

pub fn decode_detections(detections: &[Detection], width: f32, height: f32) -> Result<Vec<Bubble>> {
    let mut bubbles: Vec<Bubble> = detections
        .iter()
        .filter(|d| d.label == 0 && d.score >= 0.3)
        .filter_map(|d| {
            let bbox = d.bbox.clip(width, height)?;
            Some(Bubble {
                id: String::new(),
                bbox,
                text: String::new(),
                translation: None,
                confidence: d.score,
                reading_order: 0,
            })
        })
        .collect();
    for (i, bubble) in bubbles.iter_mut().enumerate() {
        bubble.id = format!("bubble-{}", i + 1);
    }
    bubbles.sort_by(|a, b| {
        a.bbox
            .y1
            .total_cmp(&b.bbox.y1)
            .then_with(|| b.bbox.x1.total_cmp(&a.bbox.x1))
            .then_with(|| a.id.cmp(&b.id))
    });
    for (i, b) in bubbles.iter_mut().enumerate() {
        b.reading_order = i;
    }
    Ok(bubbles)
}

pub fn validate_model_outputs(labels: &[i64], boxes: &[f32], scores: &[f32]) -> Result<()> {
    if boxes.len() != labels.len() * 4 || scores.len() != labels.len() {
        return Err(FukidashiError::TensorContract(
            "labels, boxes and scores have inconsistent N".into(),
        ));
    }
    if boxes.iter().chain(scores).any(|v| !v.is_finite()) {
        return Err(FukidashiError::TensorContract(
            "model output contains nonfinite values".into(),
        ));
    }
    Ok(())
}
