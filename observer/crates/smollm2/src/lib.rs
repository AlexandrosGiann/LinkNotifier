//! SmolLM2-135M-Instruct as a library: the modules of the upstream binary crate,
//! compiled at the crate root so their `crate::` paths resolve unchanged.
#![allow(dead_code)]

#[path = "../../../vendor/SmolLM2_135MInstruct-PureRust/src/generate.rs"]
pub mod generate;
#[path = "../../../vendor/SmolLM2_135MInstruct-PureRust/src/json.rs"]
pub mod json;
#[path = "../../../vendor/SmolLM2_135MInstruct-PureRust/src/model.rs"]
pub mod model;
#[path = "../../../vendor/SmolLM2_135MInstruct-PureRust/src/onnx.rs"]
pub mod onnx;
#[path = "../../../vendor/SmolLM2_135MInstruct-PureRust/src/pool.rs"]
pub mod pool;
#[path = "../../../vendor/SmolLM2_135MInstruct-PureRust/src/tokenizer.rs"]
pub mod tokenizer;
#[path = "../../../vendor/SmolLM2_135MInstruct-PureRust/src/unicode_tables.rs"]
mod unicode_tables;

use std::path::Path;

/// A loaded model with its tokenizer.
pub struct SmolLM {
    pub model: model::Model,
    pub tokenizer: tokenizer::Tokenizer,
}

impl SmolLM {
    /// `dir` holds `tokenizer.json` and `onnx/model_quantized.onnx`.
    pub fn load(dir: &Path, threads: usize) -> Result<SmolLM, String> {
        let tok_path = dir.join("tokenizer.json");
        let tok_text = std::fs::read_to_string(&tok_path).map_err(|e| format!("{}: {}", tok_path.display(), e))?;
        let tokenizer = tokenizer::Tokenizer::from_json(&json::parse(&tok_text)?)?;
        let file = onnx::read(&dir.join("onnx").join("model_quantized.onnx"))?;
        let model = model::Model::load(&file, threads.max(1))?;
        Ok(SmolLM { model, tokenizer })
    }

    /// One reply (greedy, at most 96 new tokens) to `(role, content)` messages.
    pub fn reply(&mut self, messages: &[(String, String)]) -> Result<String, String> {
        Ok(generate::generate(&mut self.model, &self.tokenizer, messages, |_| {})?.text)
    }
}
