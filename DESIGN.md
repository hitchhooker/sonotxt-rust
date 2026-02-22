# sonotxt private inference architecture

## overview

end-to-end encrypted TTS where server operators (us) cannot see prompts or audio.
client verifies TEE attestation, establishes MLS session over QUIC, streams encrypted
text in and receives encrypted audio out.

## threat model

**protected against:**
- server operator reading prompts/audio (us)
- network eavesdroppers
- compromised host OS
- cold boot attacks (TEE memory encryption)
- session compromise (MLS forward secrecy)

**not protected against:**
- compromised TEE hardware (nation-state level)
- side-channel attacks on TEE (spectre-class)
- client device compromise

## components

```
┌─────────────────────────────────────────────────────────────────┐
│                         CLIENT                                   │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐              │
│  │  passkey    │  │  MLS state  │  │  audio      │              │
│  │  PRF keys   │  │  (ratchet)  │  │  decoder    │              │
│  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘              │
│         │                │                │                      │
│         └────────────────┼────────────────┘                      │
│                          │                                       │
│                   QUIC + MLS                                     │
└──────────────────────────┼───────────────────────────────────────┘
                           │
                           │ (encrypted)
                           │
┌──────────────────────────┼───────────────────────────────────────┐
│                     HOST (untrusted)                             │
│                          │                                       │
│              ┌───────────┴───────────┐                          │
│              │   QUIC termination    │                          │
│              │   (pass-through)      │                          │
│              └───────────┬───────────┘                          │
│                          │                                       │
│         ┌────────────────┴────────────────┐                     │
│         │         TEE BOUNDARY            │                     │
│         │  ┌─────────────────────────┐    │                     │
│         │  │    kokoro-tee binary    │    │                     │
│         │  │  ┌─────────────────┐    │    │                     │
│         │  │  │  attestation    │    │    │                     │
│         │  │  │  (SEV-SNP/TDX)  │    │    │                     │
│         │  │  └────────┬────────┘    │    │                     │
│         │  │           │             │    │                     │
│         │  │  ┌────────┴────────┐    │    │                     │
│         │  │  │  MLS endpoint   │    │    │                     │
│         │  │  │  (key exchange) │    │    │                     │
│         │  │  └────────┬────────┘    │    │                     │
│         │  │           │             │    │                     │
│         │  │  ┌────────┴────────┐    │    │                     │
│         │  │  │  kokoro TTS     │    │    │                     │
│         │  │  │  (inference)    │    │    │                     │
│         │  │  └────────┬────────┘    │    │                     │
│         │  │           │             │    │                     │
│         │  │  ┌────────┴────────┐    │    │                     │
│         │  │  │  audio encoder  │    │    │                     │
│         │  │  │  (opus/wav)     │    │    │                     │
│         │  │  └─────────────────┘    │    │                     │
│         │  └─────────────────────────┘    │                     │
│         └─────────────────────────────────┘                     │
│                                                                  │
└──────────────────────────────────────────────────────────────────┘
```

## protocol flow

### 1. attestation + key exchange

```
client                                      TEE
   │                                         │
   │───── QUIC connect ─────────────────────>│
   │                                         │
   │<──── attestation quote + MLS KeyPackage─│
   │      (signed by TEE hardware)           │
   │                                         │
   │ verify:                                 │
   │  - quote signature (AMD/Intel root)     │
   │  - measurement matches transparency log │
   │  - KeyPackage bound to quote            │
   │                                         │
   │───── MLS Welcome + Commit ─────────────>│
   │                                         │
   │<──── MLS Commit ───────────────────────│
   │                                         │
   │      (MLS group established)            │
   │                                         │
```

### 2. streaming inference

```
client                                      TEE
   │                                         │
   │───── QUIC stream open ─────────────────>│
   │                                         │
   │───── MLS encrypt(text_chunk_1) ────────>│
   │                                         │ decrypt
   │                                         │ tokenize
   │                                         │ inference
   │                                         │ encode audio
   │<──── MLS encrypt(audio_chunk_1) ───────│
   │                                         │
   │───── MLS encrypt(text_chunk_2) ────────>│
   │<──── MLS encrypt(audio_chunk_2) ───────│
   │                                         │
   │      ... streaming continues ...        │
   │                                         │
   │───── QUIC stream close ────────────────>│
   │                                         │
   │      (MLS ratchets forward)             │
   │                                         │
```

## message formats

### attestation response

```rust
struct AttestationResponse {
    // TEE attestation quote (AMD SEV-SNP or Intel TDX)
    quote: Vec<u8>,

    // MLS KeyPackage for this TEE instance
    key_package: KeyPackage,

    // signature binding KeyPackage to quote
    binding_signature: Vec<u8>,
}
```

### inference request (inside MLS)

```rust
struct InferenceRequest {
    // unique request id
    request_id: [u8; 16],

    // voice selection
    voice: String,

    // speed multiplier
    speed: f32,

    // text to synthesize (can be chunked for streaming)
    text: String,

    // is this the final chunk?
    is_final: bool,
}
```

### inference response (inside MLS)

```rust
struct InferenceResponse {
    // matches request
    request_id: [u8; 16],

    // sequence number for ordering
    sequence: u32,

    // audio data (opus or pcm)
    audio: Vec<u8>,

    // is this the final chunk?
    is_final: bool,
}
```

## crate structure

```
sonotxt/
├── rust/                    # existing API server
│   └── src/
│       └── services/
│           └── kokoro.rs    # local inference (non-TEE)
│
├── kokoro-tee/              # NEW: TEE inference binary
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs          # TEE entry point
│       ├── attestation.rs   # SEV-SNP/TDX attestation
│       ├── mls.rs           # MLS session management
│       ├── quic.rs          # QUIC server
│       ├── inference.rs     # kokoro wrapper
│       └── protocol.rs      # message types
│
├── kokoro-client/           # NEW: client library
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── attestation.rs   # quote verification
│       ├── mls.rs           # MLS client
│       ├── quic.rs          # QUIC client
│       └── streaming.rs     # audio streaming
│
└── kokoro-common/           # NEW: shared types
    ├── Cargo.toml
    └── src/
        ├── lib.rs
        ├── protocol.rs      # InferenceRequest/Response
        └── attestation.rs   # quote types
```

## dependencies

```toml
# kokoro-tee/Cargo.toml
[dependencies]
# TTS
kokoro-tts = "0.3"

# QUIC
quinn = "0.11"
rustls = "0.23"

# MLS
openmls = "0.6"
openmls_rust_crypto = "0.2"

# attestation (pick based on hardware)
sev = "4"           # AMD SEV-SNP
# or
tdx-attest = "0.1"  # Intel TDX

# audio encoding
opus = "0.3"

# serialization
serde = { version = "1", features = ["derive"] }
bincode = "2"

# async runtime
tokio = { version = "1", features = ["full"] }
```

## build reproducibility

for attestation verification to work, builds must be reproducible.

```nix
# flake.nix (simplified)
{
  outputs = { self, nixpkgs, ... }: {
    packages.x86_64-linux.kokoro-tee = nixpkgs.rustPlatform.buildRustPackage {
      pname = "kokoro-tee";
      version = "0.1.0";
      src = ./kokoro-tee;

      # pin all inputs for reproducibility
      cargoLock.lockFile = ./Cargo.lock;

      # strip non-deterministic metadata
      stripAllList = [ "bin" ];
    };
  };
}
```

## transparency log

publish measurements to append-only log:

```rust
struct ReleaseEntry {
    version: String,
    git_commit: String,
    measurement: [u8; 48],  // SHA-384 of kernel + initrd + cmdline
    dm_verity_root: [u8; 32],
    timestamp: u64,
    signature: Vec<u8>,  // signed by release key
}
```

## open questions

1. **TEE choice**: AMD SEV-SNP vs Intel TDX?
   - SEV-SNP more mature, TDX newer but Intel-only
   - could support both with feature flags

2. **audio codec**: raw PCM vs Opus?
   - Opus = smaller, more complex
   - PCM = larger, simpler, lower latency
   - maybe offer both?

3. **MLS group size**: 1:1 or multi-party?
   - start with 1:1 (client + TEE)
   - could extend to group inference later

4. **key rotation**: how often to rotate MLS epoch?
   - every request? every N requests? time-based?
   - tradeoff: security vs performance

5. **billing**: how to bill without seeing prompts?
   - TEE reports character count (trusted)
   - or flat subscription model
