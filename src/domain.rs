use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{FukidashiError, Result};

/// A half-open rectangle in source-image pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Rect {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl Rect {
    pub fn validate(self) -> Result<Self> {
        if ![self.x1, self.y1, self.x2, self.y2]
            .iter()
            .all(|v| v.is_finite())
        {
            return Err(FukidashiError::InvalidInput(
                "rectangle coordinates must be finite".into(),
            ));
        }
        if self.x2 <= self.x1 || self.y2 <= self.y1 {
            return Err(FukidashiError::InvalidInput(
                "rectangle must have positive extent".into(),
            ));
        }
        Ok(self)
    }

    pub fn clip(self, width: f32, height: f32) -> Option<Self> {
        let clipped = Self {
            x1: self.x1.clamp(0.0, width),
            y1: self.y1.clamp(0.0, height),
            x2: self.x2.clamp(0.0, width),
            y2: self.y2.clamp(0.0, height),
        };
        (clipped.x2 > clipped.x1 && clipped.y2 > clipped.y1).then_some(clipped)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Bubble {
    pub id: String,
    pub bbox: Rect,
    pub text: String,
    pub translation: Option<String>,
    pub confidence: f32,
    pub reading_order: usize,
}

impl Bubble {
    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            return Err(FukidashiError::InvalidInput(
                "bubble id cannot be empty".into(),
            ));
        }
        self.bbox.validate()?;
        if !self.confidence.is_finite() || !(0.0..=1.0).contains(&self.confidence) {
            return Err(FukidashiError::InvalidInput(
                "bubble confidence must be between 0 and 1".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TypesetPayload {
    /// Stable editor/translation identity. Primitive clients may omit this;
    /// managed render sidecars preserve it when supplied by the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Original OCR text retained for review and source-pixel restoration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_text: Option<String>,
    /// Structural item class (for example `dialogue` or `unmatched_text`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Whether this item is preserved by strict-v1's default SFX policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserve_by_default: Option<bool>,
    /// Whether the translator marked this item as needing review. The editor
    /// surfaces this as advisory context; only an explicit user flag blocks approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_review: Option<bool>,
    /// Explicit editor flag state, retained across a rerender/reopen cycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flagged: Option<bool>,
    /// Preserve the source pixels for this item instead of fitting/rasterizing
    /// its text. This is distinct from `preserve_by_default`: an explicit
    /// replace request sets this to false even when the item is structurally
    /// unmatched text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserve_source: Option<bool>,
    /// Ordered fallback faces retained by managed render sidecars so an
    /// editor rerender can reproduce the original primary/fallback set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_font_paths: Vec<String>,
    pub bbox: Rect,
    /// Optional detector geometry. When valid, this is the containing speech
    /// bubble and takes precedence over `bbox` for the safe layout area.
    #[serde(default)]
    pub bubble_bbox: Option<Rect>,
    /// Optional DB/RT-DETR text-line geometry used as a placement anchor.
    #[serde(default)]
    pub text_bbox: Option<Rect>,
    /// Extra inset in source pixels; the renderer still enforces a minimum.
    #[serde(default)]
    pub padding: Option<f32>,
    pub text: String,
    pub font_path: Option<String>,
    pub min_font_size: Option<f32>,
    pub max_font_size: Option<f32>,
    /// Requested glyph ink. Missing/null selects contrast-aware auto ink.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_color: Option<String>,
    pub shape: Option<String>,
}

impl TypesetPayload {
    pub fn validate_text_color(&self) -> Result<()> {
        if let Some(color) = self.text_color.as_deref()
            && !matches!(color, "black" | "white")
        {
            return Err(FukidashiError::InvalidInput(format!(
                "unsupported text_color {color:?}; expected black, white, or null"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Project {
    pub schema_version: u32,
    pub pages: Vec<ProjectPage>,
    pub title: Option<String>,
    pub language: Option<String>,
    pub glossary: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectPage {
    pub id: String,
    pub image_path: String,
    pub bubbles: Vec<Bubble>,
}

impl Project {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            return Err(FukidashiError::InvalidInput(format!(
                "unsupported project schema version {}",
                self.schema_version
            )));
        }
        for page in &self.pages {
            if page.id.trim().is_empty() || page.image_path.trim().is_empty() {
                return Err(FukidashiError::InvalidInput(
                    "project page id and image_path are required".into(),
                ));
            }
            for bubble in &page.bubbles {
                bubble.validate()?;
            }
        }
        Ok(())
    }
}
