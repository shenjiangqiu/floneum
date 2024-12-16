use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use candle_core::{Device, IndexOp, Tensor};
use candle_nn::ops::softmax;
use rand::{distributions::Distribution, SeedableRng};
use tokenizers::Tokenizer;

use candle_transformers::models::whisper::{self as m, audio, Config};
use kalosm_common::{accelerated_device_if_available, CacheError};

use crate::{quantized::TextDecoderCache, Task, WhisperBuilder, WhisperLanguage};

use super::{DecodingResult, Segment};

enum ModelType {
    Quantized(crate::quantized::Whisper),
    Unquantized(m::model::Whisper),
}

impl ModelType {
    fn load(
        weights_filename: &PathBuf,
        device: &Device,
        config: Config,
        quantized: bool,
    ) -> candle_core::Result<Self> {
        if quantized {
            let vb = crate::m::quantized_model::VarBuilder::from_gguf(weights_filename, device)?;
            Ok(Self::Quantized(crate::quantized::Whisper::load(
                &vb, config,
            )?))
        } else {
            let vb = unsafe {
                candle_nn::VarBuilder::from_mmaped_safetensors(
                    &[weights_filename],
                    m::DTYPE,
                    device,
                )?
            };
            Ok(Self::Unquantized(m::model::Whisper::load(&vb, config)?))
        }
    }

    fn config(&self) -> &Config {
        match self {
            Self::Quantized(model) => &model.config,
            Self::Unquantized(model) => &model.config,
        }
    }
}

/// An error that can occur when loading a [`Whisper`](crate::Whisper) model.
#[derive(Debug, thiserror::Error)]
pub enum WhisperLoadingError {
    /// An error that can occur when trying to load a [`Whisper`](crate::Whisper) model from huggingface or a local file.
    #[error("Failed to load model from huggingface or local file: {0}")]
    DownloadingError(#[from] CacheError),
    /// An error that can occur when trying to load a [`Whisper`](crate::Whisper) model.
    #[error("Failed to load model into device: {0}")]
    LoadModel(#[from] candle_core::Error),
    /// An error that can occur when trying to load the whisper tokenizer.
    #[error("Failed to load tokenizer: {0}")]
    LoadTokenizer(tokenizers::Error),
    /// An error that can occur when trying to load the whisper config.
    #[error("Failed to load config: {0}")]
    LoadConfig(serde_json::Error),
    /// Unsupported mel filter length
    #[error("Unsupported mel filter length: {0}; only 80 and 128 are supported")]
    UnsupportedMelFilterLength(usize),
    /// Language not supported
    #[error("Language not supported: {0}")]
    UnsupportedLanguage(WhisperLanguage),
}

/// An error that can occur when running a [`Whisper`] model.
#[derive(Debug, thiserror::Error)]
pub enum WhisperError {
    /// An error that can occur when trying to run a [`Whisper`] model.
    #[error("Candle error: {0}")]
    Candle(#[from] candle_core::Error),
    /// An error that can occur when encoding or decoding for a [`Whisper`] model.
    #[error("Tokenizer error: {0}")]
    Tokenizer(tokenizers::Error),
}

pub(crate) struct WhisperInner {
    mel_filters: Vec<f32>,
    device: Device,
    decoder: Decoder,
    config: Config,
}

impl WhisperInner {
    pub(crate) fn new(
        settings: WhisperBuilder,
        weights_filename: PathBuf,
        tokenizer_filename: PathBuf,
        config_filename: PathBuf,
    ) -> Result<Self, WhisperLoadingError> {
        let device = accelerated_device_if_available()?;
        let tokenizer =
            Tokenizer::from_file(tokenizer_filename).map_err(WhisperLoadingError::LoadTokenizer)?;
        let config: Config =
            serde_json::from_str(&std::fs::read_to_string(config_filename).unwrap())
                .map_err(WhisperLoadingError::LoadConfig)?;

        let mel_bytes = match config.num_mel_bins {
            80 => include_bytes!("melfilters.bytes").as_slice(),
            128 => include_bytes!("melfilters128.bytes").as_slice(),
            nmel => return Err(WhisperLoadingError::UnsupportedMelFilterLength(nmel)),
        };
        let mut mel_filters = vec![0f32; mel_bytes.len() / 4];
        <byteorder::LittleEndian as byteorder::ByteOrder>::read_f32_into(
            mel_bytes,
            &mut mel_filters,
        );

        let model = ModelType::load(
            &weights_filename,
            &device,
            config.clone(),
            settings.model.is_quantized(),
        )?;
        let language_token = if settings.model.is_multilingual() {
            let language = settings.language.unwrap_or(WhisperLanguage::English);
            match token_id(&tokenizer, &format!("<|{language}|>")) {
                Ok(token_id) => Some(token_id),
                Err(_) => return Err(WhisperLoadingError::UnsupportedLanguage(language)),
            }
        } else {
            None
        };
        let decoder = Decoder::new(model, tokenizer, 0, &device, language_token)?;

        Ok(Self {
            mel_filters,
            device,
            decoder,
            config,
        })
    }

    pub(crate) fn transcribe(
        &mut self,
        pcm_data: Vec<f32>,
        result: tokio::sync::mpsc::UnboundedSender<Segment>,
    ) {
        let mel = audio::pcm_to_mel(&self.config, &pcm_data, &self.mel_filters);
        let mel_len = mel.len();
        let mel = Tensor::from_vec(
            mel,
            (self.config.num_mel_bins, mel_len / self.config.num_mel_bins),
            &self.device,
        )
        .unwrap();

        if let Err(err) = self
            .decoder
            .run(&mel, pcm_data.len(), Task::Transcribe, result)
        {
            tracing::error!("Error transcribing audio: {err}");
        }
    }
}

struct Decoder {
    model: ModelType,
    rng: rand::rngs::StdRng,
    tokenizer: Tokenizer,
    suppress_tokens: Tensor,
    sot_token: u32,
    transcribe_token: u32,
    translate_token: u32,
    eot_token: u32,
    no_speech_token: u32,
    no_timestamps_token: u32,
    language_token: Option<u32>,
}

impl Decoder {
    #[allow(clippy::too_many_arguments)]
    fn new(
        model: ModelType,
        tokenizer: Tokenizer,
        seed: u64,
        device: &Device,
        language_token: Option<u32>,
    ) -> candle_core::Result<Self> {
        let no_timestamps_token = token_id(&tokenizer, m::NO_TIMESTAMPS_TOKEN)?;
        // Suppress the notimestamps token when in timestamps mode.
        // https://github.com/openai/whisper/blob/e8622f9afc4eba139bf796c210f5c01081000472/whisper/decoding.py#L452
        let suppress_tokens: Vec<f32> = (0..model.config().vocab_size as u32)
            .map(|i| {
                if model.config().suppress_tokens.contains(&i) {
                    f32::NEG_INFINITY
                } else {
                    0f32
                }
            })
            .collect();
        let suppress_tokens = Tensor::new(suppress_tokens.as_slice(), device)?;
        let sot_token = token_id(&tokenizer, m::SOT_TOKEN)?;
        let transcribe_token = token_id(&tokenizer, m::TRANSCRIBE_TOKEN)?;
        let translate_token = token_id(&tokenizer, m::TRANSLATE_TOKEN)?;
        let eot_token = token_id(&tokenizer, m::EOT_TOKEN)?;
        let no_speech_token = m::NO_SPEECH_TOKENS
            .iter()
            .find_map(|token| token_id(&tokenizer, token).ok())
            .ok_or_else(|| candle_core::Error::Msg("no_speech_token not found".to_string()))?;
        Ok(Self {
            model,
            rng: rand::rngs::StdRng::seed_from_u64(seed),
            tokenizer,
            suppress_tokens,
            sot_token,
            transcribe_token,
            translate_token,
            eot_token,
            no_speech_token,
            language_token,
            no_timestamps_token,
        })
    }

    fn encode(&mut self, mel: &Tensor) -> candle_core::Result<Tensor> {
        let tensor = match &mut self.model {
            ModelType::Quantized(model) => model.encoder.forward(mel)?,
            ModelType::Unquantized(model) => model.encoder.forward(mel, true)?,
        };

        Ok(tensor)
    }

    fn decode(
        &mut self,
        audio_features: &Tensor,
        temperature: f64,
        task: Task,
        previous_tokens: &[u32],
    ) -> Result<DecodingResult, WhisperError> {
        let model = &mut self.model;
        let sample_len = model.config().max_target_positions / 2;
        let mut sum_logprob = 0f64;
        let mut no_speech_prob = f64::NAN;
        let mut tokens = vec![self.sot_token];
        if let Some(language_token) = self.language_token {
            tokens.push(language_token);
        }
        match task {
            Task::Transcribe => tokens.push(self.transcribe_token),
            Task::Translate => tokens.push(self.translate_token),
        }
        tokens.push(self.no_timestamps_token);
        tokens.extend(previous_tokens);
        // The tokens that are queued for decoding
        let mut queued_tokens = tokens.clone();
        let mut cache = TextDecoderCache::new();
        for i in 0..sample_len {
            let ys = match model {
                ModelType::Quantized(model) => {
                    let result =
                        model
                            .decoder
                            .forward(&queued_tokens, audio_features, &mut cache)?;
                    // The quantized model caches tokens so it we can remove any old tokens
                    queued_tokens.clear();
                    result
                }
                ModelType::Unquantized(model) => {
                    let tokens_t = Tensor::new(queued_tokens.as_slice(), audio_features.device())?;
                    // The model expects a batch dim but this inference loop does not handle
                    // it so we add it at this point.
                    let tokens_t = tokens_t.unsqueeze(0)?;
                    model.decoder.forward(&tokens_t, audio_features, i == 0)?
                }
            };

            // Extract the no speech probability on the first iteration by looking at the first
            // token logits and the probability for the according token.
            if i == 0 {
                let logits = match model {
                    ModelType::Quantized(model) => model.decoder.final_linear(&ys.i(..1)?)?,
                    ModelType::Unquantized(model) => model.decoder.final_linear(&ys.i(..1)?)?,
                }
                .i(0)?
                .i(0)?;
                no_speech_prob = softmax(&logits, 0)?
                    .i(self.no_speech_token as usize)?
                    .to_scalar::<f32>()? as f64;
            }

            let (_, seq_len, _) = ys.dims3()?;
            let logits = match model {
                ModelType::Quantized(model) => {
                    model.decoder.final_linear(&ys.i((..1, seq_len - 1..))?)?
                }
                ModelType::Unquantized(model) => {
                    model.decoder.final_linear(&ys.i((..1, seq_len - 1..))?)?
                }
            }
            .i(0)?
            .i(0)?;
            // TODO: Besides suppress tokens, we should apply the heuristics from
            // ApplyTimestampRules, i.e.:
            // - Timestamps come in pairs, except before EOT.
            // - Timestamps should be non-decreasing.
            // - If the sum of the probabilities of timestamps is higher than any other tokens,
            //   only consider timestamps when sampling.
            // https://github.com/openai/whisper/blob/e8622f9afc4eba139bf796c210f5c01081000472/whisper/decoding.py#L439
            let logits = logits.broadcast_add(&self.suppress_tokens)?;
            let next_token = if temperature > 0f64 {
                let prs = softmax(&(&logits / temperature)?, 0)?;
                let logits_v: Vec<f32> = prs.to_vec1()?;
                let distr = rand::distributions::WeightedIndex::new(&logits_v)
                    .expect("logits_v should not be empty or negative");
                distr.sample(&mut self.rng) as u32
            } else {
                let logits_v: Vec<f32> = logits.to_vec1()?;
                logits_v
                    .iter()
                    .enumerate()
                    .max_by(|(_, u), (_, v)| u.total_cmp(v))
                    .map(|(i, _)| i as u32)
                    .unwrap()
            };
            tokens.push(next_token);
            queued_tokens.push(next_token);
            let prob = softmax(&logits, candle_core::D::Minus1)?
                .i(next_token as usize)?
                .to_scalar::<f32>()? as f64;
            if next_token == self.eot_token || tokens.len() > model.config().max_target_positions {
                break;
            }
            sum_logprob += prob.ln();
        }
        let text = self
            .tokenizer
            .decode(&tokens, true)
            .map_err(WhisperError::Tokenizer)?;
        let avg_logprob = sum_logprob / tokens.len() as f64;

        Ok(DecodingResult {
            text,
            avg_logprob,
            no_speech_prob,
            compression_ratio: f64::NAN,
        })
    }

    fn decode_with_fallback(
        &mut self,
        audio_features: &Tensor,
        task: Task,
        previous_tokens: &[u32],
    ) -> Result<DecodingResult, WhisperError> {
        for (i, &t) in m::TEMPERATURES.iter().enumerate() {
            let dr: Result<DecodingResult, WhisperError> =
                self.decode(audio_features, t, task, previous_tokens);
            if i == m::TEMPERATURES.len() - 1 {
                return dr;
            }
            // On errors, we try again with a different temperature.
            match dr {
                Ok(dr) => {
                    let needs_fallback = dr.compression_ratio > m::COMPRESSION_RATIO_THRESHOLD
                        || dr.avg_logprob < m::LOGPROB_THRESHOLD;
                    if !needs_fallback || dr.no_speech_prob > m::NO_SPEECH_THRESHOLD {
                        return Ok(dr);
                    }
                }
                Err(err) => {
                    tracing::error!("Error running at {t}: {err}")
                }
            }
        }
        unreachable!()
    }

    fn run(
        &mut self,
        mel: &Tensor,
        audio_frames: usize,
        task: Task,
        result: tokio::sync::mpsc::UnboundedSender<Segment>,
    ) -> Result<(), WhisperError> {
        // TODO: This should be dynamic based on how much memory the model uses and how much memory is available
        const MAX_CHUNKS: usize = 1;

        let (_, content_frames) = mel.dims2()?;
        let mut seek = 0;
        let start_time = Instant::now();
        let mut chunk_indices = Vec::new();
        let mut chunked = Vec::new();
        // Keep looping until we have all the chunks we need
        while seek < content_frames {
            // Take a chunk up to the maximum size
            chunk_indices.clear();
            chunked.clear();
            while chunk_indices.len() < MAX_CHUNKS && seek < content_frames {
                let remaining_frames = content_frames - seek;
                let segment_size = usize::min(remaining_frames, m::N_FRAMES);
                // If the new frame doesn't fit into a perfect chunk, just include it in the next chunk
                if remaining_frames < m::N_FRAMES && !chunk_indices.is_empty() {
                    break;
                }
                chunk_indices.push(seek..seek + segment_size);
                let mel_segment = mel.narrow(1, seek, segment_size)?;
                chunked.push(mel_segment);
                seek += segment_size;
            }

            // Encode all of the chunks
            let batched_mel_segment = Tensor::stack(&chunked, 0)?;
            let batched_audio_features = self.encode(&batched_mel_segment)?;
            let split = batched_audio_features.chunk(chunk_indices.len(), 0)?;

            // Tokens that are remaining in the last chunk's sentence fragment
            let mut tokens_in_sentence_fragment = Vec::new();

            for (audio_features, range) in split.iter().zip(chunk_indices.iter()) {
                let segment_size = range.end - range.start;
                let end = range.end;
                let time_offset = (end * m::HOP_LENGTH) as f64 / m::SAMPLE_RATE as f64;

                let segment_duration =
                    (segment_size * m::HOP_LENGTH) as f64 / m::SAMPLE_RATE as f64;
                let dr =
                    self.decode_with_fallback(audio_features, task, &tokens_in_sentence_fragment)?;
                tokens_in_sentence_fragment.clear();
                if dr.no_speech_prob > m::NO_SPEECH_THRESHOLD
                    && dr.avg_logprob < m::LOGPROB_THRESHOLD
                {
                    tracing::trace!("no speech detected, skipping {end} {dr:?}");
                    continue;
                }

                // Grab any text that was in the previous sentence fragment
                if let Some(index) = dr.text.char_indices().rev().find_map(|(idx, c)| {
                    if c == '.' || c == '?' || c == '!' {
                        Some(idx)
                    } else {
                        None
                    }
                }) {
                    let text_after_last_sentence = &dr.text[index + 1..];
                    let tokens = self
                        .tokenizer
                        .encode(text_after_last_sentence, false)
                        .map_err(WhisperError::Tokenizer)?;
                    tokens_in_sentence_fragment.extend(tokens.get_ids());
                };

                let elapsed = start_time.elapsed();
                let remaining = Duration::from_millis(
                    ((elapsed.as_millis() as usize / seek) * (content_frames - seek)) as u64,
                );
                let progress = end as f32 / content_frames as f32;
                let segment = Segment {
                    sample_range: (range.start * m::HOP_LENGTH)
                        ..audio_frames.min(range.end * m::HOP_LENGTH),
                    start: time_offset,
                    duration: segment_duration,
                    remaining_time: remaining,
                    elapsed_time: elapsed,
                    progress,
                    result: dr,
                };

                if let Err(err) = result.send(segment) {
                    tracing::error!("Error sending segment: {err}");
                    break;
                }
            }
        }

        Ok(())
    }
}

pub fn token_id(tokenizer: &Tokenizer, token: &str) -> candle_core::Result<u32> {
    match tokenizer.token_to_id(token) {
        None => candle_core::bail!("no token-id for {token}"),
        Some(id) => Ok(id),
    }
}
