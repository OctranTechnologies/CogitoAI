//! Private model wire-protocol adapter contract.
//!
//! A provider facade can select one of these adapters using its model catalog.
//! This is how a future OpenCode adapter can route individual Zen/Go models to
//! Responses, Chat Completions, or Messages protocols without changing the
//! agent loop or exposing wire representations in public runtime types.

use super::{ModelRequest, ModelResponse, ModelStreamEvent, ProviderError};

pub(crate) trait ProtocolAdapter: Send + Sync {
    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError>;

    fn stream(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<ModelResponse, ProviderError>;
}
