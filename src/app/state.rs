//! Operation-identified application state transitions.
//!
//! Completions are accepted only for the currently active operation ID, which
//! prevents cancelled or stale asynchronous work from replacing newer state.

use crate::api::{ModelsCheck, ResponseMetadata};
use crate::domain::GeneratedImage;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OperationId(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationKind {
    Connecting,
    Generating,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConnectionResult {
    pub http_status: u16,
    pub model_present: bool,
    pub metadata: ResponseMetadata,
}

impl From<ModelsCheck> for ConnectionResult {
    fn from(check: ModelsCheck) -> Self {
        Self {
            http_status: check.http_status,
            model_present: check.model_present,
            metadata: check.metadata,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SuccessResult {
    Connection(ConnectionResult),
    Generation(GeneratedImage),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ApplicationState {
    Idle,
    Connecting {
        operation: OperationId,
    },
    Generating {
        operation: OperationId,
    },
    Success {
        operation: OperationId,
        result: SuccessResult,
    },
    Cancelled {
        operation: OperationId,
    },
    Error {
        operation: Option<OperationId>,
        message: String,
    },
}

impl ApplicationState {
    fn active_operation(&self) -> Option<OperationId> {
        match self {
            Self::Connecting { operation } | Self::Generating { operation } => Some(*operation),
            Self::Idle | Self::Success { .. } | Self::Cancelled { .. } | Self::Error { .. } => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct StateMachine {
    state: ApplicationState,
    next_operation: u64,
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new(ApplicationState::Idle)
    }
}

impl StateMachine {
    pub(crate) fn new(state: ApplicationState) -> Self {
        Self {
            state,
            next_operation: 0,
        }
    }

    pub(crate) fn state(&self) -> &ApplicationState {
        &self.state
    }

    pub(crate) fn begin(&mut self, kind: OperationKind) -> OperationId {
        self.next_operation = self.next_operation.wrapping_add(1);
        if self.next_operation == 0 {
            self.next_operation = 1;
        }
        let operation = OperationId(self.next_operation);
        self.state = match kind {
            OperationKind::Connecting => ApplicationState::Connecting { operation },
            OperationKind::Generating => ApplicationState::Generating { operation },
        };
        operation
    }

    pub(crate) fn is_active(&self, operation: OperationId) -> bool {
        self.state.active_operation() == Some(operation)
    }

    pub(crate) fn succeed(&mut self, operation: OperationId, result: SuccessResult) -> bool {
        if !self.is_active(operation) {
            return false;
        }
        self.state = ApplicationState::Success { operation, result };
        true
    }

    pub(crate) fn fail(&mut self, operation: OperationId, message: String) -> bool {
        if !self.is_active(operation) {
            return false;
        }
        self.state = ApplicationState::Error {
            operation: Some(operation),
            message,
        };
        true
    }

    pub(crate) fn reject(&mut self, message: String) {
        self.state = ApplicationState::Error {
            operation: None,
            message,
        };
    }

    pub(crate) fn cancel(&mut self) -> Option<OperationId> {
        let operation = self.state.active_operation()?;
        self.state = ApplicationState::Cancelled { operation };
        Some(operation)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::domain::PersistedImage;
    use crate::generation::{GenerationInput, GenerationOptions, OutputFormat};

    fn generated_image() -> GeneratedImage {
        GeneratedImage::new(
            PersistedImage {
                path: PathBuf::from("/tmp/generated.png"),
                width: 2,
                height: 3,
                file_size: 24,
                output_format: OutputFormat::Png,
            },
            Duration::from_millis(250),
            ResponseMetadata::default(),
            GenerationInput::new("test prompt", GenerationOptions::default())
                .expect("test generation input should be valid"),
        )
    }

    #[test]
    fn transitions_from_idle_through_connecting_to_success() {
        let mut machine = StateMachine::default();
        assert_eq!(machine.state(), &ApplicationState::Idle);

        let operation = machine.begin(OperationKind::Connecting);
        assert_eq!(machine.state(), &ApplicationState::Connecting { operation });

        let result = SuccessResult::Connection(ConnectionResult {
            http_status: 200,
            model_present: true,
            metadata: ResponseMetadata::default(),
        });
        assert!(machine.succeed(operation, result.clone()));
        assert_eq!(
            machine.state(),
            &ApplicationState::Success { operation, result }
        );
    }

    #[test]
    fn transitions_generating_operation_to_error() {
        let mut machine = StateMachine::default();
        let operation = machine.begin(OperationKind::Generating);

        assert!(machine.fail(operation, "gateway unavailable".to_owned()));
        assert_eq!(
            machine.state(),
            &ApplicationState::Error {
                operation: Some(operation),
                message: "gateway unavailable".to_owned(),
            }
        );
    }

    #[test]
    fn validation_failure_uses_the_explicit_error_state() {
        let mut machine = StateMachine::default();

        machine.reject("prompt is required".to_owned());

        assert_eq!(
            machine.state(),
            &ApplicationState::Error {
                operation: None,
                message: "prompt is required".to_owned(),
            }
        );
    }

    #[test]
    fn cancellation_is_explicit_and_rejects_late_completion() {
        let mut machine = StateMachine::default();
        let operation = machine.begin(OperationKind::Generating);

        assert_eq!(machine.cancel(), Some(operation));
        assert_eq!(machine.state(), &ApplicationState::Cancelled { operation });
        assert!(!machine.succeed(operation, SuccessResult::Generation(generated_image())));
        assert_eq!(machine.state(), &ApplicationState::Cancelled { operation });
    }

    #[test]
    fn stale_result_cannot_overwrite_a_newer_operation() {
        let mut machine = StateMachine::default();
        let stale = machine.begin(OperationKind::Generating);
        let current = machine.begin(OperationKind::Connecting);

        assert!(!machine.succeed(stale, SuccessResult::Generation(generated_image())));
        assert_eq!(
            machine.state(),
            &ApplicationState::Connecting { operation: current }
        );

        let current_result = SuccessResult::Connection(ConnectionResult {
            http_status: 200,
            model_present: true,
            metadata: ResponseMetadata::default(),
        });
        assert!(machine.succeed(current, current_result.clone()));
        assert_eq!(
            machine.state(),
            &ApplicationState::Success {
                operation: current,
                result: current_result,
            }
        );
    }
}
