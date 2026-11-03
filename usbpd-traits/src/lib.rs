//! USB PD library traits.
//!
//! Provides a driver trait that allows to add support for various USB PD PHYs.
#![cfg_attr(not(test), no_std)]
#![warn(missing_docs)]
use core::future::Future;

/// The start-of-packet ordered set a message is sent or received with, per PD 3.2 Sec. 5.6.1.2.
///
/// `Sop` addresses the port partner, `SopPrime` and `SopDoublePrime` the cable plugs. The debug variants are for
/// vendor debug traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Sop {
    /// SOP: port to port.
    #[default]
    Sop,
    /// SOP': port to the cable plug nearest the VCONN source.
    SopPrime,
    /// SOP'': port to the far cable plug.
    SopDoublePrime,
    /// SOP'_Debug.
    SopPrimeDebug,
    /// SOP''_Debug.
    SopDoublePrimeDebug,
}

impl Sop {
    /// Number of SOP types.
    pub const COUNT: usize = 5;

    /// All SOP types, in the order of [`Sop::index`].
    pub const ALL: [Sop; Self::COUNT] = [
        Sop::Sop,
        Sop::SopPrime,
        Sop::SopDoublePrime,
        Sop::SopPrimeDebug,
        Sop::SopDoublePrimeDebug,
    ];

    /// A dense index in `0..Sop::COUNT`, for per-SOP state.
    pub const fn index(self) -> usize {
        match self {
            Sop::Sop => 0,
            Sop::SopPrime => 1,
            Sop::SopDoublePrime => 2,
            Sop::SopPrimeDebug => 3,
            Sop::SopDoublePrimeDebug => 4,
        }
    }

    /// Whether this SOP addresses a cable plug (SOP', SOP'' and their debug variants).
    pub const fn is_cable(self) -> bool {
        !matches!(self, Sop::Sop)
    }
}

/// A frame received by the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct RxFrame {
    /// The SOP the frame was received with.
    pub sop: Sop,
    /// Number of bytes written to the receive buffer (message header and data, no CRC).
    pub len: usize,
}

/// Error type of driver receive operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum DriverRxError {
    /// Received message discarded, e.g. due to CRC errors.
    Discarded,

    /// Hard Reset received before or during reception.
    HardReset,

    /// Cable Reset received before or during reception.
    ///
    /// Resets the SOP' and SOP'' message state only. Drivers that cannot tell it apart from a Hard Reset report
    /// `HardReset`.
    CableReset,

    /// The port partner detached. The policy engine stops and returns [`Detached`](DriverRxError::Detached) to the
    /// caller instead of retrying.
    Detached,
}

/// Error type of driver transmit operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum DriverTxError {
    /// Concurrent receive in progress or excessive noise on the line.
    Discarded,

    /// Hard Reset received before or during transmission.
    HardReset,

    /// The port partner detached.
    Detached,
}

/// Driver trait, through which the protocol layer talks to the PHY.
pub trait Driver {
    /// If this is `true`, the protocol layer will not send its own
    /// GoodCRC messages and will instead rely on the hardware.
    const HAS_AUTO_GOOD_CRC: bool = false;

    /// If this is `true`, the hardware automatically retries transmission
    /// when no GoodCRC is received. The protocol layer will skip its own
    /// retry loop and call driver.transmit() directly. On success, it
    /// maintains protocol state (TX message counter) without calling
    /// wait_for_good_crc(), since the hardware already verified GoodCRC.
    const HAS_AUTO_RETRY: bool = false;

    /// Wait for availability of VBus voltage.
    fn wait_for_vbus(&mut self) -> impl Future<Output = ()>;

    /// Receive a packet with any of the SOP types the driver is set up to receive.
    ///
    /// A driver that only receives SOP returns [`Sop::Sop`] in every frame.
    fn receive(&mut self, buffer: &mut [u8]) -> impl Future<Output = Result<RxFrame, DriverRxError>>;

    /// Transmit a packet with the given SOP.
    fn transmit(&mut self, sop: Sop, data: &[u8]) -> impl Future<Output = Result<(), DriverTxError>>;

    /// Transmit a hard reset signal.
    fn transmit_hard_reset(&mut self) -> impl Future<Output = Result<(), DriverTxError>>;
}
