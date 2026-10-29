//! Loading the five models. Every model except Whistle is optional: when its
//! files are missing the observer still runs and that part of the notes is
//! left out.
//!
//! Expected layout under `--models DIR` (see download_models.ps1):
//!
//! ```text
//! whistle.cact                       Cactus Whistle (speech to text)
//! smollm2/tokenizer.json             SmolLM2-135M-Instruct (summaries)
//! smollm2/onnx/model_quantized.onnx
//! mobileclip/compiled-fp32-v1/...    MobileCLIP-S0 (what is on screen)
//! mobileclip/text-bank/...
//! minilm/manifest.json ...           paraphrase-multilingual-MiniLM-L12-v2 (topics, dedup, search)
//! needle3.cact or needle2.cact       Needle (action items, decisions, questions)
//! ```

use std::path::PathBuf;

pub enum Needle {
    V3(needle_infer::v3_engine::V3Engine),
    V2(needle_infer::v2_engine::V2Engine),
}

impl Needle {
    /// Returns the tool-call JSON and, when the model has a confidence head,
    /// how sure it is.
    pub fn call(&self, query: &str, tools: &str) -> (Option<String>, Option<f32>) {
        match self {
            Needle::V3(e) => {
                let (res, p) = e.run_scored(query, tools);
                (needle_infer::v3_engine::extract_tool_call(&res.text), p)
            }
            Needle::V2(e) => (e.run(query, tools).tool_call, None),
        }
    }
}

pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn whistle(&self) -> PathBuf {
        self.root.join("whistle.cact")
    }
    pub fn smollm(&self) -> PathBuf {
        self.root.join("smollm2")
    }
    pub fn mobileclip(&self) -> PathBuf {
        self.root.join("mobileclip")
    }
    pub fn minilm(&self) -> PathBuf {
        self.root.join("minilm")
    }
    pub fn needle(&self) -> Option<PathBuf> {
        ["needle3.cact", "needle2.cact"].iter().map(|f| self.root.join(f)).find(|p| p.is_file())
    }
}

pub fn load_whistle(p: &Paths) -> Result<whistle::Model, String> {
    whistle::Model::open(p.whistle()).map_err(|e| format!("{}: {}", p.whistle().display(), e))
}

pub fn load_minilm(p: &Paths, threads: usize) -> Result<minilm::MiniLM, String> {
    minilm::MiniLM::load(&p.minilm(), threads)
}

pub fn load_smollm(p: &Paths, threads: usize) -> Result<smollm2::SmolLM, String> {
    smollm2::SmolLM::load(&p.smollm(), threads)
}

pub fn load_mobileclip(p: &Paths, threads: usize) -> Result<(mobileclip_s0::MobileClip, Option<mobileclip_s0::TextBank>), String> {
    let root = p.mobileclip();
    let model = mobileclip_s0::MobileClip::load(&root, threads).map_err(|e| format!("{}: {}", root.display(), e))?;
    let bank = mobileclip_s0::TextBank::load(&root).ok();
    Ok((model, bank))
}

pub fn load_needle(p: &Paths) -> Result<Needle, String> {
    let path = p.needle().ok_or_else(|| format!("no needle3.cact or needle2.cact in {}", p.root.display()))?;
    if path.file_name().is_some_and(|f| f == "needle3.cact") {
        needle_infer::v3_engine::V3Engine::load(&path).map(Needle::V3).map_err(|e| format!("{}: {:?}", path.display(), e))
    } else {
        needle_infer::v2_engine::V2Engine::load(&path).map(Needle::V2).map_err(|e| format!("{}: {}", path.display(), e))
    }
}

/// Which model files are present, for `observer check` and the app.
pub fn report(p: &Paths) -> Vec<(&'static str, bool, PathBuf)> {
    let needle = p.needle().unwrap_or_else(|| p.root.join("needle3.cact"));
    vec![
        ("whistle", p.whistle().is_file(), p.whistle()),
        ("smollm2", p.smollm().join("onnx").join("model_quantized.onnx").is_file(), p.smollm()),
        ("mobileclip", p.mobileclip().join("compiled-fp32-v1").join("plan.json").is_file(), p.mobileclip()),
        ("minilm", p.minilm().join("manifest.json").is_file(), p.minilm()),
        ("needle", needle.is_file(), needle),
    ]
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut d, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        d += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        d / (na.sqrt() * nb.sqrt())
    }
}
