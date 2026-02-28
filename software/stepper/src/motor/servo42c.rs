use embedded_io::Write;
use esp_hal::{delay::Delay, uart::Uart, Blocking};
use mks_servo42_rs::{self, Driver, RotationDirection};

use super::{Motor, MotorError, Position};

/// Default UART read timeout in milliseconds.
const READ_TIMEOUT_MS: u32 = 100;

/// Motor speed for go_to moves (0..=127).
const MOVE_SPEED: u8 = 50;

/// Maximum command size in the mks-servo42-rs protocol.
const MAX_CMD_LEN: usize = 16;

pub struct Servo42cMotor<'a> {
    driver: Driver,
    uart: Uart<'a, Blocking>,
    delay: Delay,
}

impl<'a> Servo42cMotor<'a> {
    pub fn new(uart: Uart<'a, Blocking>, delay: Delay) -> Self {
        Self {
            driver: Driver::default(),
            uart,
            delay,
        }
    }

    /// Write a command and read the response into `buf`.
    /// Returns the number of bytes read.
    fn transact(&mut self, cmd: &[u8], buf: &mut [u8]) -> Result<usize, MotorError> {
        self.uart.write_all(cmd).map_err(|_| MotorError::Communication)?;
        self.uart.flush().map_err(|_| MotorError::Communication)?;

        // Give the motor time to respond.
        self.delay.delay_millis(READ_TIMEOUT_MS);

        let n = self.uart.read(buf).map_err(|_| MotorError::Communication)?;
        Ok(n)
    }

    fn read_pulse_count(&mut self) -> Result<Position, MotorError> {
        let mut cmd_buf = [0u8; MAX_CMD_LEN];
        let cmd = self.driver.read_pulse_count();
        let len = cmd.len();
        cmd_buf[..len].copy_from_slice(cmd);

        let mut resp = [0u8; 16];
        let n = self.transact(&cmd_buf[..len], &mut resp)?;
        mks_servo42_rs::parse_pulse_count_response(&resp[..n])
            .map_err(|_| MotorError::Communication)
    }
}

impl Motor for Servo42cMotor<'_> {
    fn set_enabled(&mut self, enabled: bool) {
        let mut cmd_buf = [0u8; MAX_CMD_LEN];
        let cmd = self.driver.enable_motor(enabled);
        let len = cmd.len();
        cmd_buf[..len].copy_from_slice(cmd);

        let mut resp = [0u8; 16];
        if let Ok(n) = self.transact(&cmd_buf[..len], &mut resp) {
            if let Ok(r) = mks_servo42_rs::parse_success_response(&resp[..n]) {
                if r.is_success() {
                    defmt::info!("Motor {}.", if enabled { "enabled" } else { "disabled" });
                    return;
                }
            }
        }
        defmt::warn!("Failed to {} motor", if enabled { "enable" } else { "disable" });
    }

    fn go_to(&mut self, target: Position) -> Result<Position, MotorError> {
        let current = self.read_pulse_count()?;
        let delta = target - current;

        if delta == 0 {
            return Ok(current);
        }

        let direction = if delta > 0 {
            RotationDirection::Clockwise
        } else {
            RotationDirection::CounterClockwise
        };
        let pulses = delta.unsigned_abs();

        let mut cmd_buf = [0u8; MAX_CMD_LEN];
        let cmd = self.driver.run_motor(direction, MOVE_SPEED, pulses)
            .map_err(|_| MotorError::OutOfRange)?;
        let len = cmd.len();
        cmd_buf[..len].copy_from_slice(cmd);

        let mut resp = [0u8; 16];
        self.transact(&cmd_buf[..len], &mut resp)?;

        // Poll until motion is complete.
        loop {
            self.delay.delay_millis(50);
            let pos = self.read_pulse_count()?;
            if (pos - target).unsigned_abs() < 10 {
                return Ok(pos);
            }
        }
    }

    fn stop(&mut self) {
        let mut cmd_buf = [0u8; MAX_CMD_LEN];
        let cmd = self.driver.stop();
        let len = cmd.len();
        cmd_buf[..len].copy_from_slice(cmd);

        let mut resp = [0u8; 16];
        let _ = self.transact(&cmd_buf[..len], &mut resp);
    }

    fn position(&mut self) -> Result<Position, MotorError> {
        self.read_pulse_count()
    }
}
