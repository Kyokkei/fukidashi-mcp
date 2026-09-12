use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelManifest {
    pub detector: PathBuf,
    pub ocr: PathBuf,
    pub lama: PathBuf,
}

impl ModelManifest {
    pub fn from_root(root: &Path) -> Self {
        Self {
            detector: root.join("detection/detector-v4-s_int8.onnx"),
            ocr: root.join("ocr/manga-ocr-mobile-onnx/encoder.onnx"),
            lama: root.join("inpainting/lama-manga-dynamic.onnx"),
        }
    }
}
