#[allow(dead_code)]
pub mod step_dir;

#[cfg(feature = "servo42c")]
pub mod servo42c;

/// Position in microsteps from zero (signed).
pub type Position = i32;

/// Errors that can occur during motor operations.
#[derive(Debug, defmt::Format)]
#[allow(dead_code)]
pub enum MotorError {
    Communication,
    OutOfRange,
}

/// Common interface for motor backends.
#[allow(dead_code)]
pub trait Motor {
    fn set_enabled(&mut self, enabled: bool);
    fn go_to(&mut self, position: Position) -> Result<Position, MotorError>;
    fn stop(&mut self);
    fn position(&mut self) -> Result<Position, MotorError>;
}
