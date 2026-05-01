use clap::Parser;

/// The AI Veto Node (Moral Advocate) configuration.
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Port to listen on
    #[arg(short, long, default_value_t = 50060)]
    pub port: u16,

    /// Ollama model to use
    #[arg(short, long, default_value = "qwen3.5:4b")]
    pub model: String,

    /// Ollama host (must include scheme, e.g., http://localhost)
    #[arg(long, default_value = "http://localhost")]
    pub ollama_host: String,

    /// Ollama port
    #[arg(long, default_value_t = 11434)]
    pub ollama_port: u16,

    /// Enable deep reasoning (Think Mode). Fatal to Raft latency on old
    /// hardware.
    #[arg(long, default_value_t = false)]
    pub think: bool,

    /// VRAM Pinning: Keep model in memory (seconds). Use -1 for infinite.
    #[arg(long, default_value_t = -1)]
    pub keep_alive: i64,

    /// Hardware Latency Bound: Maximum tokens to generate (num_predict).
    #[arg(long, default_value_t = 256)]
    pub max_tokens: u32,
}
