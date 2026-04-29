use esp_hal::{delay::Delay, gpio::Output};

use super::{Motor, MotorError, Position};

pub struct StepDirMotor<'a> {
    stp: Output<'a>,
    dir: Output<'a>,
    en: Output<'a>,
    delay: Delay,
    position: Position,
    step_delay_us: u32,
}

impl<'a> StepDirMotor<'a> {
    pub fn new(
        stp: Output<'a>,
        dir: Output<'a>,
        en: Output<'a>,
        delay: Delay,
        step_delay_us: u32,
    ) -> Self {
        Self {
            stp,
            dir,
            en,
            delay,
            position: 0,
            step_delay_us,
        }
    }

    /// Change the step pulse delay (lower = faster).
    pub fn set_speed(&mut self, step_delay_us: u32) {
        self.step_delay_us = step_delay_us;
    }

    /// Execute a single step pulse in the current direction.
    fn step_once(&mut self) {
        self.stp.set_high();
        self.delay.delay_micros(self.step_delay_us);
        self.stp.set_low();
        self.delay.delay_micros(self.step_delay_us);
    }
}

impl Motor for StepDirMotor<'_> {
    fn set_enabled(&mut self, enabled: bool) {
        // Active low: LOW = enabled.
        if enabled {
            self.en.set_low();
        } else {
            self.en.set_high();
        }
    }

    fn go_to(&mut self, target: Position) -> Result<Position, MotorError> {
        let delta = target - self.position;

        // Motor is mounted reversed relative to the turntable, so the dir
        // GPIO polarity is flipped vs. the step/dir driver's nominal sense.
        if delta > 0 {
            self.dir.set_high(); // clockwise (turntable frame)
        } else {
            self.dir.set_low(); // counter-clockwise (turntable frame)
        }

        let steps = delta.unsigned_abs();
        for _ in 0..steps {
            self.step_once();
            if delta > 0 {
                self.position += 1;
            } else {
                self.position -= 1;
            }
        }

        Ok(self.position)
    }

    fn stop(&mut self) {
        // Step/dir motor stops when we stop pulsing — nothing to do.
    }

    fn position(&mut self) -> Result<Position, MotorError> {
        Ok(self.position)
    }
}
