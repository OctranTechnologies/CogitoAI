use harness_core::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRequest {
    pub prompt: String,
    pub max_output_tokens: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelResponse {
    pub text: String,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderCapability {
    TextGeneration,
    ToolCalling,
    Streaming,
}

pub trait ModelProvider: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> &[ProviderCapability];
    fn complete(&self, request: ModelRequest) -> Result<ModelResponse, Error>;
}
