//! Whether the configured model can see images.
//!
//! This decides two things: what the system prompt tells the agent to do with a
//! browser (read the accessibility tree, or also look at it), and whether the
//! `see()` tool is available at all. Getting it wrong in either direction is
//! costly — a blind model that takes screenshots wastes wakes producing images
//! nobody reads, and a sighted model told to avoid them works harder than it
//! needs to on visual pages.
//!
//! There is no reliable capability endpoint across OpenAI-compatible providers,
//! so this matches on model names and can always be overridden per profile.

/// Substrings that identify a model family as multimodal. Matched
/// case-insensitively against the model id, which is usually prefixed by the
/// provider (`anthropic/claude-sonnet-4.5`, `openai/gpt-4o`).
const SIGHTED: &[&str] = &[
    // OpenAI
    "gpt-4o",
    "gpt-4.1",
    "gpt-4-turbo",
    "gpt-4-vision",
    "gpt-5",
    "o3",
    "o4",
    "chatgpt-4o",
    // Anthropic — every Claude 3 and later accepts images
    "claude-3",
    "claude-4",
    "claude-sonnet",
    "claude-opus",
    "claude-haiku",
    // Google
    "gemini",
    // Open weights
    "llama-3.2-11b",
    "llama-3.2-90b",
    "llama-4",
    "pixtral",
    "llava",
    "qwen-vl",
    "qwen2-vl",
    "qwen2.5-vl",
    "internvl",
    "moondream",
    "minicpm-v",
    "phi-3.5-vision",
    "phi-4-multimodal",
    "molmo",
    "idefics",
    "grok-2-vision",
    "grok-4",
    "mistral-medium-3",
    "step-1v",
    "glm-4v",
    "yi-vision",
];

/// Names that contain a sighted substring but are text-only, so they must be
/// checked first.
const BLIND_EXCEPTIONS: &[&str] = &[
    "gpt-4o-mini-tts",
    "gpt-4o-transcribe",
    "gpt-4o-mini-transcribe",
    "gpt-4o-audio",
    "gpt-4o-mini-audio",
    "gpt-4o-realtime",
    "gpt-4o-mini-realtime",
    "o3-mini",
    "o4-mini-deep-research",
    "gemini-embedding",
];

/// Best guess at whether `model` accepts image input.
pub fn model_sees(model: &str) -> bool {
    let name = model.to_lowercase();
    if BLIND_EXCEPTIONS.iter().any(|blind| name.contains(blind)) {
        return false;
    }
    SIGHTED.iter().any(|sighted| name.contains(sighted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_multimodal_models() {
        for model in [
            "gpt-4o",
            "gpt-4.1",
            "openai/gpt-4.1-mini",
            "anthropic/claude-sonnet-4.5",
            "claude-3-5-haiku-20241022",
            "google/gemini-2.5-pro",
            "qwen2.5-vl-72b",
            "mistralai/pixtral-12b",
        ] {
            assert!(model_sees(model), "{model} should be treated as sighted");
        }
    }

    #[test]
    fn recognizes_text_only_models() {
        for model in [
            "gpt-3.5-turbo",
            "llama-3.3-70b-versatile",
            "qwen2.5-coder-32b",
            "deepseek-chat",
            "mistral-large",
            "codestral",
        ] {
            assert!(!model_sees(model), "{model} should be treated as blind");
        }
    }

    #[test]
    fn excludes_audio_and_mini_reasoning_variants() {
        // These contain a sighted substring but take no images.
        assert!(!model_sees("gpt-4o-audio-preview"));
        assert!(!model_sees("gpt-4o-mini-tts"));
        assert!(!model_sees("o3-mini"));
        // …while the base models they resemble do.
        assert!(model_sees("o3"));
        assert!(model_sees("gpt-4o"));
    }

    #[test]
    fn is_case_insensitive() {
        assert!(model_sees("GPT-4O"));
        assert!(model_sees("Anthropic/Claude-Opus-4"));
    }
}
