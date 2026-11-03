//! Implementation of a dual role device

use core::marker::PhantomData;

use usbpd_traits::Driver;

use crate::sink::device_policy_manager::SinkDpm;
use crate::sink::policy_engine::{Error as SinkError, Sink};
use crate::source::device_policy_manager::SourceDpm;
use crate::source::policy_engine::{Error as SourceError, Source};
use crate::timers::Timer;
use crate::{PowerRole, RunResult};

/// Errors that can occur in the either the sink or source policy engine state machine.
#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Error {
    /// Error while operating as a Sink
    Sink(SinkError),
    /// Error while operating as a Source
    Source(SourceError),
}

/// Dual Role Port that will automatically undergo role swaps,
/// using the defined functions in the two `DualRoleDevicePolicyManagers`
#[derive(Debug)]
pub struct DualRolePort<DRIVER, TIMER, DPM>
where
    DRIVER: Driver,
    TIMER: Timer,
    DPM: SourceDpm + SinkDpm,
{
    device_policy_manager: DPM,
    driver: DRIVER,
    timer: PhantomData<TIMER>,
}

impl<DRIVER, TIMER, DPM> DualRolePort<DRIVER, TIMER, DPM>
where
    DRIVER: Driver,
    TIMER: Timer,
    DPM: SourceDpm + SinkDpm,
{
    /// Create a new dual role policy engine with a given `driver`.
    pub fn new(driver: DRIVER, device_policy_manager: DPM) -> Self {
        Self {
            device_policy_manager,
            driver,
            timer: PhantomData,
        }
    }

    /// Run the sink's state machine continuously.
    ///
    /// The loop is only broken for unrecoverable errors, for example if the port partner is unresponsive.
    ///
    /// NOTE: This function consumes the Dual Role Port Driver. A new driver must be constructed after an error is returned
    pub async fn run(mut self, initial_role: PowerRole) -> Result<(), Error> {
        let mut role = initial_role;
        let mut role_swapped = false;

        loop {
            let result;
            (result, self.driver, self.device_policy_manager) = match role {
                PowerRole::Source => {
                    Self::run_source_until_swap(self.driver, self.device_policy_manager, role_swapped).await?
                }
                PowerRole::Sink => Self::run_sink_until_swap(self.driver, self.device_policy_manager).await?,
            };

            (role, role_swapped) = match result {
                RunResult::SwapToSink => (PowerRole::Sink, true),
                RunResult::SwapToSource => (PowerRole::Source, true),
                // Start over in the initial role on the next attach.
                RunResult::Detached => (initial_role, false),
            };
        }
    }

    /// Run as a sink until a power role swap to Source or a detach.
    async fn run_sink_until_swap(
        driver: DRIVER,
        device_policy_manager: DPM,
    ) -> Result<(RunResult, DRIVER, DPM), Error> {
        let mut sink = Sink::<DRIVER, TIMER, DPM>::new_dual_role(driver, device_policy_manager);

        let result = sink.run().await.map_err(Error::Sink)?;
        let (driver, dpm) = sink.deconstruct();
        Ok((result, driver, dpm))
    }

    /// Run as a source until a power role swap to Sink or a detach.
    async fn run_source_until_swap(
        driver: DRIVER,
        device_policy_manager: DPM,
        role_swapped: bool,
    ) -> Result<(RunResult, DRIVER, DPM), Error> {
        let mut source = Source::<DRIVER, TIMER, DPM>::new_dual_role(driver, device_policy_manager, role_swapped);

        let result = source.run().await.map_err(Error::Source)?;
        let (driver, dpm) = source.deconstruct();
        Ok((result, driver, dpm))
    }
}
