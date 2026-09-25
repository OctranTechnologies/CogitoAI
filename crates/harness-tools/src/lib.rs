use std::collections::BTreeMap;

use harness_core::Error;
use harness_policy::{authorize, Permission, Policy};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRequest {
    pub name: String,
    pub arguments: BTreeMap<String, String>,
}

impl ToolRequest {
    pub fn new(name: impl Into<String>, arguments: BTreeMap<String, String>) -> Self {
        Self {
            name: name.into(),
            arguments,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolResult {
    pub output: String,
    pub metadata: BTreeMap<String, String>,
}

pub struct ToolContext<'a> {
    pub policy: &'a dyn Policy,
    pub working_directory: &'a std::path::Path,
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn required_permission(&self) -> Permission;
    fn execute(&self, context: &ToolContext<'_>, request: ToolRequest)
        -> Result<ToolResult, Error>;
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, Error> {
        let tool = self
            .tools
            .iter()
            .find(|tool| tool.name() == request.name)
            .ok_or_else(|| Error::Tool {
                tool: request.name.clone(),
                message: "tool is not registered".to_owned(),
            })?;
        authorize(context.policy, tool.required_permission())?;
        tool.execute(context, request)
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|tool| tool.name()).collect()
    }
}
