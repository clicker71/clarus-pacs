// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

#![deny(unsafe_code)]

use std::fmt;

/// Errors that can occur during pipeline stage execution.
#[derive(Debug)]
pub enum PipelineError {
    /// A stage-specific error with a human-readable description.
    Stage(String),
    /// Configuration error.
    Config(String),
    /// Wrapped I/O error.
    Io(std::io::Error),
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stage(msg) => write!(f, "pipeline stage error: {msg}"),
            Self::Config(msg) => write!(f, "pipeline config error: {msg}"),
            Self::Io(err) => write!(f, "pipeline I/O error: {err}"),
        }
    }
}

impl std::error::Error for PipelineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for PipelineError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// A single stage in the processing pipeline.
///
/// Each stage receives raw bytes and returns processed bytes.
/// Stages are composable: the output of one stage becomes the input of the next.
pub trait PipelineStage: Send + Sync {
    /// Process a chunk of bytes through this stage.
    fn process(&self, bytes: Vec<u8>) -> Result<Vec<u8>, PipelineError>;
}

/// A linear pipeline of [`PipelineStage`]s.
///
/// Stages are executed in order. If any stage fails, the pipeline short-circuits.
pub struct Pipeline {
    stages: Vec<Box<dyn PipelineStage>>,
}

impl Pipeline {
    /// Create a new pipeline from a list of stages.
    pub fn new(stages: Vec<Box<dyn PipelineStage>>) -> Self {
        Self { stages }
    }

    /// Run all stages sequentially on the given input.
    pub fn run(&self, bytes: Vec<u8>) -> Result<Vec<u8>, PipelineError> {
        let mut data = bytes;
        for stage in &self.stages {
            data = stage.process(data)?;
        }
        Ok(data)
    }
}

/// A no-op stage that passes bytes through unchanged.
///
/// Useful as a placeholder or for testing.
pub struct NoopStage;

impl PipelineStage for NoopStage {
    fn process(&self, bytes: Vec<u8>) -> Result<Vec<u8>, PipelineError> {
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_stage_passes_bytes_through() {
        let stage = NoopStage;
        let input = vec![1, 2, 3];
        let result = stage.process(input.clone()).unwrap();
        assert_eq!(result, input);
    }

    #[test]
    fn pipeline_runs_stages_in_order() {
        struct AppendStage(u8);
        impl PipelineStage for AppendStage {
            fn process(&self, mut bytes: Vec<u8>) -> Result<Vec<u8>, PipelineError> {
                bytes.push(self.0);
                Ok(bytes)
            }
        }

        let pipeline = Pipeline::new(vec![
            Box::new(AppendStage(4)),
            Box::new(AppendStage(5)),
            Box::new(AppendStage(6)),
        ]);
        let result = pipeline.run(vec![1, 2, 3]).unwrap();
        assert_eq!(result, vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn empty_pipeline_is_identity() {
        let pipeline = Pipeline::new(vec![]);
        let input = vec![42];
        let result = pipeline.run(input.clone()).unwrap();
        assert_eq!(result, input);
    }

    #[test]
    fn pipeline_error_propagates() {
        struct FailingStage;
        impl PipelineStage for FailingStage {
            fn process(&self, _bytes: Vec<u8>) -> Result<Vec<u8>, PipelineError> {
                Err(PipelineError::Stage("fail".into()))
            }
        }

        let pipeline = Pipeline::new(vec![Box::new(FailingStage)]);
        let err = pipeline.run(vec![]).unwrap_err();
        assert!(matches!(err, PipelineError::Stage(_)));
    }

    #[test]
    fn pipeline_error_display() {
        let err = PipelineError::Stage("test".into());
        assert_eq!(err.to_string(), "pipeline stage error: test");

        let err = PipelineError::Config("bad".into());
        assert_eq!(err.to_string(), "pipeline config error: bad");

        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let err = PipelineError::Io(io_err);
        assert!(err.to_string().contains("pipeline I/O error: gone"));
    }
}
