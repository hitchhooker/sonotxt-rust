use kokoro_tts::{KokoroTts, Voice};
use std::time::Instant;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("loading kokoro model...");
    let start = Instant::now();
    let tts = KokoroTts::new("models/kokoro-v1.0.int8.onnx", "models/voices.bin").await?;
    println!("model loaded in {:?}", start.elapsed());

    let text = "Hello! This is a test of Kokoro text to speech running in Rust. Pretty cool, right?";

    println!("\ngenerating speech for: {}", text);
    let start = Instant::now();
    let (samples, took) = tts.synth(text, Voice::AfBella(1.0)).await?;
    let gen_time = start.elapsed();
    let audio_duration = samples.len() as f32 / 24000.0;

    println!("generated {:.2}s audio in {:?}", audio_duration, gen_time);
    println!("realtime factor: {:.1}x", audio_duration / gen_time.as_secs_f32());
    println!("kokoro internal timing: {:?}", took);

    // save to wav
    let wav = encode_wav(&samples);
    std::fs::write("test_kokoro.wav", &wav)?;
    println!("\nsaved to test_kokoro.wav ({} bytes)", wav.len());

    Ok(())
}

fn encode_wav(samples: &[f32]) -> Vec<u8> {
    let mut buf = Vec::new();
    let sample_rate = 24000u32;
    let data_size = (samples.len() * 2) as u32;
    let file_size = 36 + data_size;

    // RIFF header
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&file_size.to_le_bytes());
    buf.extend_from_slice(b"WAVE");

    // fmt chunk
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());  // PCM
    buf.extend_from_slice(&1u16.to_le_bytes());  // mono
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes());
    buf.extend_from_slice(&16u16.to_le_bytes());

    // data chunk
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_size.to_le_bytes());

    for &s in samples {
        let i = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        buf.extend_from_slice(&i.to_le_bytes());
    }

    buf
}
