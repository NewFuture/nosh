//! Model implementations.

pub mod attn;
pub mod llama;
pub mod qwen35;
mod tensor_attn;

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use candle_core::quantized::gguf_file;
use candle_core::{Device, Result, Tensor};

use crate::tokenizer::Tok;
use attn::KvDtype;
use llama::{Llama, LoadOptions, PrepackStats};
use qwen35::Qwen35;

fn read_metadata(path: &Path) -> Result<gguf_file::Content> {
    gguf_file::Content::read(&mut BufReader::new(File::open(path)?))
}

pub(crate) fn validate_tokenizer(path: &Path, arch: &str, tok: &Tok) -> Result<Vec<bool>> {
    let content = read_metadata(path)?;
    if arch == "llama" {
        let config = llama::LlamaConfig::from_gguf(&content)?;
        if tok.vocab_size() != config.vocab {
            candle_core::bail!(
                "tokenizer vocab {} does not match model vocab {}",
                tok.vocab_size(),
                config.vocab
            );
        }
        return Ok(vec![true; config.vocab]);
    }
    if arch != "qwen35" {
        candle_core::bail!("unsupported GGUF architecture '{arch}'");
    }
    let config = qwen35::Qwen35Config::from_gguf(&content)?;
    let tokens = content
        .metadata
        .get("tokenizer.ggml.tokens")
        .ok_or_else(|| candle_core::Error::Msg("GGUF tokenizer vocabulary missing".into()))?
        .to_vec()?;
    let mut valid = vec![false; config.vocab];
    for (text, id) in tok.vocabulary() {
        let id = id as usize;
        if id >= tokens.len() || tokens[id].to_string()? != text.as_str() {
            candle_core::bail!("Qwen35 tokenizer/GGUF vocabulary mismatch at token {id}");
        }
        valid[id] = true;
    }
    if let Some(types) = content.metadata.get("tokenizer.ggml.token_type") {
        let types = types.to_vec()?;
        if types.len() != valid.len() {
            candle_core::bail!("Qwen35 token-type vocabulary length mismatch");
        }
        for (allowed, kind) in valid.iter_mut().zip(types) {
            if kind.to_i32()? == 5 {
                *allowed = false;
            }
        }
    }
    Ok(valid)
}

pub(crate) enum Model {
    Llama(Box<Llama>),
    Qwen35(Box<Qwen35>),
}

pub(crate) fn architecture(path: &Path) -> Result<String> {
    let content = read_metadata(path)?;
    Ok(content
        .metadata
        .get("general.architecture")
        .ok_or_else(|| candle_core::Error::Msg("GGUF architecture missing".into()))?
        .to_string()?
        .clone())
}

#[cfg(feature = "cuda")]
pub(crate) fn cuda_memory_estimate(path: &Path, context: usize, chunk: usize) -> Result<u64> {
    match architecture(path)?.as_str() {
        "llama" => llama::cuda_memory_estimate(path, context, chunk),
        "qwen35" => qwen35::cuda_memory_estimate(path, context, chunk),
        arch => candle_core::bail!("unsupported GGUF architecture '{arch}'"),
    }
}

impl Model {
    pub(crate) fn load(
        path: &Path,
        arch: &str,
        context: usize,
        opts: LoadOptions,
        device: &Device,
    ) -> Result<Self> {
        match arch {
            "llama" => Ok(Self::Llama(Box::new(Llama::load(
                path, context, opts, device,
            )?))),
            "qwen35" => Ok(Self::Qwen35(Box::new(Qwen35::load(
                path, context, opts, device,
            )?))),
            _ => candle_core::bail!("unsupported GGUF architecture '{arch}'"),
        }
    }

    pub(crate) fn layers(&self) -> usize {
        match self {
            Self::Llama(model) => model.config().n_layer,
            Self::Qwen35(model) => model.config().n_layer,
        }
    }

    pub(crate) fn vocab(&self) -> usize {
        match self {
            Self::Llama(model) => model.config().vocab,
            Self::Qwen35(model) => model.config().vocab,
        }
    }

    pub(crate) fn max_context(&self) -> usize {
        match self {
            Self::Llama(model) => model.max_context(),
            Self::Qwen35(model) => model.max_context(),
        }
    }

    pub(crate) fn kv_dtype(&self) -> KvDtype {
        match self {
            Self::Llama(model) => model.kv_dtype(),
            Self::Qwen35(model) => model.kv_dtype(),
        }
    }

    pub(crate) fn prepack_stats(&self) -> PrepackStats {
        match self {
            Self::Llama(model) => model.prepack_stats(),
            Self::Qwen35(model) => model.prepack_stats(),
        }
    }

    /// Returns the prefix for which *all* model state remains valid.
    pub(crate) fn prepare_prefix(&mut self, len: usize) -> usize {
        match self {
            Self::Llama(model) => {
                model.truncate(len);
                model.kv_len()
            }
            Self::Qwen35(model) => model.prepare_prefix(len),
        }
    }

    pub(crate) fn reset(&mut self) {
        match self {
            Self::Llama(model) => model.truncate(0),
            Self::Qwen35(model) => model.reset(),
        }
    }

    pub(crate) fn forward(&mut self, ids: &[u32]) -> Result<Tensor> {
        let result = match self {
            Self::Llama(model) => model.forward(ids),
            Self::Qwen35(model) => model.forward(ids),
        };
        if result.is_err() {
            self.reset();
        }
        result
    }
}
