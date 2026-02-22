// local kokoro tts inference service
use kokoro_tts::{KokoroTts, Voice};
use std::sync::Arc;
use tokio::sync::RwLock;
use crate::error::{ApiError, Result};

pub struct KokoroService {
    tts: Arc<RwLock<KokoroTts>>,
}

impl KokoroService {
    pub async fn new(model_path: &str, voices_path: &str) -> Result<Self> {
        let tts = KokoroTts::new(model_path, voices_path)
            .await
            .map_err(|e| ApiError::InternalError)?;

        Ok(Self {
            tts: Arc::new(RwLock::new(tts)),
        })
    }

    /// synthesize text to audio samples (f32 PCM, 24kHz mono)
    pub async fn synthesize(&self, text: &str, voice_id: &str, speed: f32) -> Result<(Vec<f32>, std::time::Duration)> {
        let voice = parse_voice(voice_id, speed)?;
        let tts = self.tts.read().await;

        let (samples, duration) = tts
            .synth(text, voice)
            .await
            .map_err(|e| ApiError::InternalError)?;

        Ok((samples, duration))
    }

    /// synthesize and encode to wav bytes
    pub async fn synthesize_wav(&self, text: &str, voice_id: &str, speed: f32) -> Result<Vec<u8>> {
        let (samples, _duration) = self.synthesize(text, voice_id, speed).await?;
        let wav = encode_wav(&samples, 24000)?;
        Ok(wav)
    }
}

fn parse_voice(voice_id: &str, speed: f32) -> Result<Voice> {
    // map voice ids like "af_bella" to Voice enum
    let voice = match voice_id {
        "af_bella" => Voice::AfBella(speed),
        "af_nicole" => Voice::AfNicole(speed),
        "af_sarah" => Voice::AfSarah(speed),
        "af_sky" => Voice::AfSky(speed),
        "af_nova" => Voice::AfNova(speed),
        "af_river" => Voice::AfRiver(speed),
        "af_jessica" => Voice::AfJessica(speed),
        "af_heart" => Voice::AfHeart(speed),
        "af_alloy" => Voice::AfAlloy(speed),
        "af_aoede" => Voice::AfAoede(speed),
        "af_kore" => Voice::AfKore(speed),
        "am_adam" => Voice::AmAdam(speed),
        "am_michael" => Voice::AmMichael(speed),
        "am_eric" => Voice::AmEric(speed),
        "am_liam" => Voice::AmLiam(speed),
        "am_puck" => Voice::AmPuck(speed),
        "am_fenrir" => Voice::AmFenrir(speed),
        "am_onyx" => Voice::AmOnyx(speed),
        "am_echo" => Voice::AmEcho(speed),
        "am_santa" => Voice::AmSanta(speed),
        "bf_emma" => Voice::BfEmma(speed),
        "bf_alice" => Voice::BfAlice(speed),
        "bf_lily" => Voice::BfLily(speed),
        "bf_isabella" => Voice::BfIsabella(speed),
        "bm_george" => Voice::BmGeorge(speed),
        "bm_daniel" => Voice::BmDaniel(speed),
        "bm_lewis" => Voice::BmLewis(speed),
        "bm_fable" => Voice::BmFable(speed),
        // japanese
        "jf_nezumi" => Voice::JfNezumi(speed),
        "jf_tebukuro" => Voice::JfTebukuro(speed),
        "jf_alpha" => Voice::JfAlpha(speed),
        "jf_gongitsune" => Voice::JfGongitsune(speed),
        "jm_kumo" => Voice::JmKumo(speed),
        // chinese
        "zf_xiaoxiao" => Voice::ZfXiaoxiao(speed),
        "zf_xiaoni" => Voice::ZfXiaoni(speed),
        "zf_xiaobei" => Voice::ZfXiaobei(speed),
        "zf_xiaoyi" => Voice::ZfXiaoyi(speed),
        "zm_yunyang" => Voice::ZmYunyang(speed),
        "zm_yunxi" => Voice::ZmYunxi(speed),
        "zm_yunxia" => Voice::ZmYunxia(speed),
        "zm_yunjian" => Voice::ZmYunjian(speed),
        // other languages
        "ff_siwis" => Voice::FfSiwis(speed),  // french
        "ef_dora" => Voice::EfDora(speed),    // spanish
        "em_alex" => Voice::EmAlex(speed),
        "em_santa" => Voice::EmSanta(speed),
        "hf_alpha" => Voice::HfAlpha(speed),  // hindi
        "hf_beta" => Voice::HfBeta(speed),
        "hm_psi" => Voice::HmPsi(speed),
        "hm_omega" => Voice::HmOmega(speed),
        "if_sara" => Voice::IfSara(speed),    // italian
        "im_nicola" => Voice::ImNicola(speed),
        "pf_dora" => Voice::PfDora(speed),    // portuguese
        "pm_alex" => Voice::PmAlex(speed),
        "pm_santa" => Voice::PmSanta(speed),
        _ => return Err(ApiError::InvalidRequest(format!("unknown voice: {}", voice_id))),
    };
    Ok(voice)
}

fn encode_wav(samples: &[f32], sample_rate: u32) -> Result<Vec<u8>> {
    use std::io::Cursor;

    let mut buf = Vec::new();
    let mut cursor = Cursor::new(&mut buf);

    // wav header
    let data_size = (samples.len() * 2) as u32; // 16-bit samples
    let file_size = 36 + data_size;

    // RIFF header
    cursor.get_mut().extend_from_slice(b"RIFF");
    cursor.get_mut().extend_from_slice(&file_size.to_le_bytes());
    cursor.get_mut().extend_from_slice(b"WAVE");

    // fmt chunk
    cursor.get_mut().extend_from_slice(b"fmt ");
    cursor.get_mut().extend_from_slice(&16u32.to_le_bytes()); // chunk size
    cursor.get_mut().extend_from_slice(&1u16.to_le_bytes());  // PCM format
    cursor.get_mut().extend_from_slice(&1u16.to_le_bytes());  // mono
    cursor.get_mut().extend_from_slice(&sample_rate.to_le_bytes());
    cursor.get_mut().extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    cursor.get_mut().extend_from_slice(&2u16.to_le_bytes());  // block align
    cursor.get_mut().extend_from_slice(&16u16.to_le_bytes()); // bits per sample

    // data chunk
    cursor.get_mut().extend_from_slice(b"data");
    cursor.get_mut().extend_from_slice(&data_size.to_le_bytes());

    // convert f32 samples to i16
    for &sample in samples {
        let s = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
        cursor.get_mut().extend_from_slice(&s.to_le_bytes());
    }

    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_kokoro_synthesis() {
        let service = KokoroService::new(
            "models/kokoro-v1.0.int8.onnx",
            "models/voices.bin"
        ).await.unwrap();

        let wav = service.synthesize_wav("Hello world!", "af_bella", 1.0).await.unwrap();
        assert!(!wav.is_empty());
        assert_eq!(&wav[0..4], b"RIFF");
    }
}
