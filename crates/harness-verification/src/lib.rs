use harness_core::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VerificationCommand {
    Test,
    Lint,
    Typecheck,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationRequest {
    pub working_directory: std::path::PathBuf,
    pub commands: Vec<VerificationCommand>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationReport {
    pub command: VerificationCommand,
    pub passed: bool,
    pub output: String,
}

pub trait Verifier: Send + Sync {
    fn verify(&self, request: &VerificationRequest) -> Result<Vec<VerificationReport>, Error>;
}
