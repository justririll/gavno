//! The two MediaPipe networks run on the CPU with tract.

use crate::vision::{self, Letterbox, Palm, Roi};
use std::path::{Path, PathBuf};
use tract_onnx::prelude::*;

type Net = std::sync::Arc<TypedRunnableModel>;

pub const PALM_FILE: &str = "palm_detection_mediapipe_2023feb.onnx";
pub const HAND_FILE: &str = "handpose_estimation_mediapipe_2023feb.onnx";

fn load(path: &Path, side: usize) -> Result<Net, String> {
    let error = |e: TractError| format!("{}: {e}", path.display());
    tract_onnx::onnx()
        .model_for_path(path).map_err(error)?
        .with_input_fact(0, f32::fact([1, side, side, 3]).into()).map_err(error)?
        .into_optimized().map_err(error)?
        .into_runnable().map_err(error)
}

fn floats(tensor: &Tensor) -> Result<Vec<f32>, String> {
    let view = tensor.to_plain_array_view::<f32>().map_err(|e| e.to_string())?;
    Ok(view.iter().copied().collect())
}

/// `$GAVNO_CAM_MODELS`, the installed location, then the working directory.
pub fn model_dir() -> Option<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(dir) = std::env::var("GAVNO_CAM_MODELS") { dirs.push(PathBuf::from(dir)); }
    dirs.push("/usr/local/share/gavno/models".into());
    dirs.push("models".into());
    dirs.into_iter().find(|d| d.join(PALM_FILE).is_file() && d.join(HAND_FILE).is_file())
}

pub struct Hand {
    /// Landmarks in frame pixels.
    pub landmarks: [[f32; 2]; 21],
    pub confidence: f32,
    pub roi: Roi,
}

pub struct Models {
    palm: Net,
    hand: Net,
    anchors: Vec<[f32; 2]>,
    input: Vec<f32>,
}

impl Models {
    pub fn load(dir: &Path) -> Result<Self, String> {
        Ok(Self {
            palm: load(&dir.join(PALM_FILE), vision::PALM_SIZE)?,
            hand: load(&dir.join(HAND_FILE), vision::HAND_SIZE)?,
            anchors: vision::palm_anchors(),
            input: Vec::new(),
        })
    }

    /// Finds the most likely palm in the whole frame.
    pub fn detect_palm(&mut self, rgb: &[u8], width: usize, height: usize) -> Result<Option<Palm>, String> {
        let fit: Letterbox = vision::letterbox(rgb, width, height, &mut self.input);
        let tensor = Tensor::from_shape(&[1, vision::PALM_SIZE, vision::PALM_SIZE, 3], &self.input).map_err(|e| e.to_string())?;
        let outputs = self.palm.run(tvec!(tensor.into())).map_err(|e| e.to_string())?;
        // Boxes are 2016x18 and scores 2016x1; tell them apart by size.
        let (mut boxes, mut scores) = (None, None);
        for output in outputs.iter() {
            let data = floats(output)?;
            if data.len() == vision::ANCHORS * 18 { boxes = Some(data); }
            else if data.len() == vision::ANCHORS { scores = Some(data); }
        }
        let (Some(boxes), Some(scores)) = (boxes, scores) else { return Err("unexpected palm detector outputs".into()) };
        Ok(vision::decode_palm(&boxes, &scores, &self.anchors, fit, (width, height), 0.55))
    }

    /// Runs the landmark network on a region of interest.
    pub fn landmarks(&mut self, rgb: &[u8], width: usize, height: usize, roi: Roi) -> Result<Hand, String> {
        vision::warp_roi(rgb, width, height, &roi, &mut self.input);
        let tensor = Tensor::from_shape(&[1, vision::HAND_SIZE, vision::HAND_SIZE, 3], &self.input).map_err(|e| e.to_string())?;
        let outputs = self.hand.run(tvec!(tensor.into())).map_err(|e| e.to_string())?;
        // Outputs: 63 screen landmarks, hand presence, handedness, 63 world landmarks.
        let first = floats(&outputs[0])?;
        let confidence = floats(&outputs[1])?.first().copied().unwrap_or(0.0);
        if first.len() < 63 { return Err("unexpected hand landmark outputs".into()); }
        let landmarks = vision::landmarks_to_frame(&first, &roi);
        Ok(Hand { landmarks, confidence, roi })
    }
}
