//! The protocol layer is controlled by the policy engine, and commands the PHY layer.
//!
//! Handles
//! - construction of messages,
//! - message timers and timeouts,
//! - message retry counters,
//! - extended message chunking and assembly,
//! - reset operation,
//! - error handling,
//! - state behaviour.

#[cfg(feature = "epr")]
pub mod epr;
pub mod message;

use core::future::Future;
use core::marker::PhantomData;

use embassy_futures::select::{Either, select};
use heapless::{Deque, Vec};
use message::Message;
use message::data::{Data, request};
#[cfg(any(feature = "epr", test))]
use message::extended::extended_control::ExtendedControlMessageType;
use message::header::{ControlMessageType, DataMessageType, ExtendedMessageType, Header, MessageType};
use usbpd_traits::{Driver, DriverRxError, DriverTxError, Sop};

use crate::PowerRole;
use crate::counters::{Counter, CounterType, Error as CounterError};
use crate::protocol_layer::message::data::source_capabilities::SourceCapabilities;
#[cfg(any(feature = "epr", test))]
use crate::protocol_layer::message::extended::Extended;
use crate::protocol_layer::message::extended::chunked::{MAX_EXTENDED_MSG_CHUNK_LEN, MAX_EXTENDED_MSG_LEN};
use crate::protocol_layer::message::{ParseError, Payload};
use crate::timers::{Timer, TimerType};

/// Maximum message size including headers and payload.
const MAX_MESSAGE_SIZE: usize = 272;

/// Capacity of the pending received-message queue.
///
/// Overflow indicates a misbehaving peer. Per PD 3.2 Sec. 6.12.2.2. If full drop oldest message per PD 3.2 Sec. 6.11.
const PENDING_RX_CAPACITY: usize = 2;

/// Size of the message header in bytes.
const MSG_HEADER_SIZE: usize = 2;

/// Size of the extended message header in bytes.
const EXT_HEADER_SIZE: usize = 2;

/// Errors that can occur in the protocol layer.
#[derive(thiserror::Error, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum ProtocolError {
    /// An error occured during data reception.
    #[error("RX error")]
    RxError(#[from] RxError),
    /// An error occured during data transmission.
    #[error("TX error")]
    TxError(#[from] TxError),
    /// Transmission failed after the maximum number of allowed retries.
    #[error("transmit retries (`{0}`) exceeded")]
    TransmitRetriesExceeded(u8),
    /// An unexpected message was received.
    #[error("unexpected message")]
    UnexpectedMessage,
}

/// Errors that can occur during reception of data.
#[derive(thiserror::Error, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum RxError {
    /// Port partner requested soft reset.
    #[error("soft reset")]
    SoftReset,
    /// Driver reported a hard reset.
    #[error("hard reset")]
    HardReset,
    /// A timeout during message reception.
    #[error("receive timeout")]
    ReceiveTimeout,
    /// An unsupported message was received.
    #[error("unsupported message")]
    UnsupportedMessage,
    /// A message parsing error occured.
    #[error("parse error")]
    ParseError(#[from] ParseError),
    /// The received acknowledgement does not match the last transmitted message's ID.
    #[error("wrong tx id `{0}` acknowledged")]
    AcknowledgeMismatch(u8),
    /// The driver reported that the port partner detached.
    #[error("detached")]
    Detached,
}

/// Errors that can occur during transmission of data.
#[derive(thiserror::Error, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum TxError {
    /// Driver reported a hard reset.
    #[error("hard reset")]
    HardReset,
    /// The driver reported that the port partner detached.
    #[error("detached")]
    Detached,
    /// unchunked_extended_messages_supported must be false (library uses chunked mode).
    #[error("unchunked extended messages not supported")]
    UnchunkedExtendedMessagesNotSupported,
    /// AVS voltage LSB 2 bits must be zero per PD 3.2 Tab. 6.26.
    #[error("AVS voltage alignment invalid")]
    AvsVoltageAlignmentInvalid,
    /// A chunked extended message was not sent completely: the receiver requested a wrong chunk, stopped
    /// requesting chunks, or sent another message in between (per PD 3.2 Sec. 6.12.2.1.3.8).
    #[error("chunked transmission failed")]
    ChunkedTransmissionFailed,
}

/// Message ID state of one SOP, per PD 3.2 Sec. 6.7.1: each SOP* has its own `MessageIDCounter` and stored
/// `MessageID`.
#[derive(Debug, Clone, Copy)]
struct SopCounters {
    rx_message: Option<Counter>,
    tx_message: Counter,
}

impl Default for SopCounters {
    fn default() -> Self {
        Self {
            rx_message: None,
            tx_message: Counter::new(CounterType::MessageId),
        }
    }
}

#[derive(Debug)]
struct Counters {
    _busy: Counter,
    _caps: Counter, // Unused, optional.
    _discover_identity: Counter,
    sop: [SopCounters; Sop::COUNT],
    retry: Counter,
}

impl Default for Counters {
    fn default() -> Self {
        Counters {
            _busy: Counter::new(CounterType::Busy),
            _caps: Counter::new(CounterType::Caps),
            _discover_identity: Counter::new(CounterType::DiscoverIdentity),
            sop: [SopCounters::default(); Sop::COUNT],
            retry: Counter::new(CounterType::Retry),
        }
    }
}

impl Counters {
    fn of(&self, sop: Sop) -> &SopCounters {
        &self.sop[sop.index()]
    }

    fn of_mut(&mut self, sop: Sop) -> &mut SopCounters {
        &mut self.sop[sop.index()]
    }
}

/// Queue of messages received while this port was awaiting GoodCRC for its own transmission.
///
/// Per PD 3.2 Sec. 6.12.2.3.1 the receive state machine runs concurrently, so messages are acknowledged and stored for
/// later delivery instead of being dropped.
#[derive(Debug, Default)]
struct PendingRx<const N: usize> {
    // The queue of pending received messages.
    queue: Deque<Message, N>,
}

impl<const N: usize> PendingRx<N> {
    /// Park a message, dropping the oldest one on overflow (per PD 3.2 Sec. 6.11).
    fn push(&mut self, message: Message) {
        if self.queue.is_full() {
            _ = self.queue.pop_front();
            warn!("Pending received-message queue full, dropping oldest message");
        }
        _ = self.queue.push_back(message);
    }

    /// Take the oldest parked message, if any.
    fn pop(&mut self) -> Option<Message> {
        self.queue.pop_front()
    }

    /// Discard all parked messages (per PD 3.2 Sec. 6.12.2.3.2, layer reset for receive).
    fn clear(&mut self) {
        self.queue.clear();
    }
}

/// The USB PD protocol layer.
///
/// Implements message-level state including message IDs, retries,
/// `GoodCRC` handling (also via parked extended messages) and hard resets.
#[derive(Debug)]
pub(crate) struct ProtocolLayer<DRIVER: Driver, TIMER: Timer> {
    driver: DRIVER,
    counters: Counters,
    default_header: Header,
    extended_rx_buffer: Vec<u8, MAX_MESSAGE_SIZE>,
    extended_rx_expected: Option<(ExtendedMessageType, u16, u8)>,
    pending_rx: PendingRx<PENDING_RX_CAPACITY>,
    _timer: PhantomData<TIMER>,
}

impl<DRIVER: Driver, TIMER: Timer> ProtocolLayer<DRIVER, TIMER> {
    /// Create a new protocol layer from a driver and default header.
    pub fn new(driver: DRIVER, default_header: Header) -> Self {
        Self {
            driver,
            counters: Default::default(),
            default_header,
            extended_rx_buffer: Vec::new(),
            extended_rx_expected: None,
            pending_rx: PendingRx::default(),
            _timer: PhantomData,
        }
    }

    pub fn deconstruct(self) -> DRIVER {
        self.driver
    }

    /// Reset the protocol layer, for all SOPs.
    pub fn reset(&mut self) {
        self.counters = Default::default();
        self.pending_rx.clear();
        // Half-assembled chunked messages must not survive a layer reset.
        self.reset_chunked_rx();
    }

    /// Reset the message ID state of the cable SOPs (SOP', SOP'' and their debug variants), as after a Cable Reset
    /// (per PD 3.2 Sec. 6.7.1).
    pub fn reset_cable(&mut self) {
        for sop in Sop::ALL.into_iter().filter(|sop| sop.is_cable()) {
            *self.counters.of_mut(sop) = SopCounters::default();
        }
    }

    /// The SOP `MessageIDCounter`, for building message headers.
    ///
    /// [`transmit`](Self::transmit) stamps the counter of the message's SOP into its header anyway.
    fn tx_counter(&self) -> Counter {
        self.counters.of(Sop::Sop).tx_message
    }

    /// The header template for messages sent with `sop`.
    ///
    /// Per PD 3.2 Sec. 6.2.1.1.4 and 6.2.1.1.7, in SOP'/SOP'' messages bit 8 is the Cable Plug flag (0: sent by a
    /// port) and bit 5 is reserved, so both are cleared.
    fn header_template(&self, sop: Sop) -> Header {
        match sop {
            Sop::Sop => self.default_header,
            _ => Header(self.default_header.0 & !((1 << 8) | (1 << 5))),
        }
    }

    /// Allows tests to access the driver directly.
    #[cfg(test)]
    pub fn driver(&mut self) -> &mut DRIVER {
        &mut self.driver
    }

    /// Access the default header directly.
    pub fn header(&self) -> &Header {
        &self.default_header
    }

    fn get_message_buffer() -> [u8; MAX_MESSAGE_SIZE] {
        [0u8; MAX_MESSAGE_SIZE]
    }

    /// Get a timer future for a given type.
    pub fn get_timer(timer_type: TimerType) -> impl Future<Output = ()> {
        TimerType::get_timer::<TIMER>(timer_type)
    }

    /// Receive a simple (non-chunked) message from the driver.
    /// Used by wait_for_good_crc to avoid recursion with chunked message handling.
    async fn receive_simple(&mut self) -> Result<Message, RxError> {
        loop {
            let mut buffer = Self::get_message_buffer();

            let Some(frame) = self.driver_receive(&mut buffer).await? else {
                continue;
            };

            let message = Message::from_bytes(&buffer[..frame.len])?.with_sop(frame.sop);
            return Ok(message);
        }
    }

    /// Receive one frame from the driver.
    ///
    /// Returns `Ok(None)` for frames to skip: discarded ones, and a Cable Reset (which resets the cable SOPs).
    async fn driver_receive(&mut self, buffer: &mut [u8]) -> Result<Option<usbpd_traits::RxFrame>, RxError> {
        match self.driver.receive(buffer).await {
            Ok(frame) => Ok(Some(frame)),
            Err(DriverRxError::Discarded) => Ok(None),
            Err(DriverRxError::CableReset) => {
                trace!("Cable Reset received");
                self.reset_cable();
                Ok(None)
            }
            Err(DriverRxError::HardReset) => Err(RxError::HardReset),
            Err(DriverRxError::Detached) => Err(RxError::Detached),
        }
    }

    /// Wait until a GoodCrc message is received, or a timeout occurs.
    ///
    /// Per PD 3.2 Sec. 6.12.2.3.1 the receive state machine runs concurrently with the
    /// transmit state machine, so a port partner may transmit while this port
    /// awaits GoodCRC. Such messages are acknowledged and parked for later
    /// delivery instead of aborting the wait (per PD 3.2 Sec. 6.12.2.2.1.5, Tab. 8.2).
    ///
    /// Only a GoodCRC with the SOP of our own transmission (`sop`) acknowledges it.
    async fn wait_for_good_crc(&mut self, sop: Sop) -> Result<(), RxError> {
        trace!("Wait for GoodCrc");

        let timeout_fut = Self::get_timer(TimerType::CRCReceive);
        let receive_fut = async {
            loop {
                let message = self.receive_simple().await?;

                if matches!(
                    message.header.message_type(),
                    MessageType::Control(ControlMessageType::GoodCRC)
                ) {
                    if message.sop != sop {
                        trace!("Ignoring GoodCrc for {:?} while waiting on {:?}", message.sop, sop);
                        continue;
                    }
                    let tx_message = &mut self.counters.of_mut(sop).tx_message;
                    trace!(
                        "Received GoodCrc, TX message count: {}, expected: {}",
                        message.header.message_id(),
                        tx_message.value()
                    );
                    if message.header.message_id() == tx_message.value() {
                        // Per PD 3.2 Sec. 6.7.1.1
                        _ = tx_message.increment();
                        self.counters.retry.reset();
                        return Ok(());
                    } else {
                        return Err(RxError::AcknowledgeMismatch(message.header.message_id()));
                    }
                } else {
                    // Extended frames that reach here parsed in `receive_simple`
                    // (single-chunk or unchunked). They park alongside control and
                    // data messages for later delivery. True multi-chunk frames
                    // fail parsing in `receive_simple` before reaching this path
                    // (deferred: D-RDv-3, see docs/spec-conformance-report.md).
                    self.park_received_message(message).await?;
                }
            }
        };

        match select(timeout_fut, receive_fut).await {
            Either::First(_) => Err(RxError::ReceiveTimeout),
            Either::Second(receive_result) => receive_result,
        }
    }

    /// Park a regular (not `GoodCRC`) message received while awaiting `GoodCRC` for our own transmission.
    ///
    /// Acknowledges the message (per PD 3.2 Sec. 6.7.1.2), skipping GoodCRC emission for drivers with automatic acknowledgement.
    /// Duplicates are only acknowledged again, not parked twice (per PD 3.2 Sec. 6.7.1.2).
    ///
    /// Per PD 3.2 Sec. 6.2.1.1.5, the sender of a GoodCRC Message Shall set the Specification Revision field to match the
    /// revision of the message being acknowledged. Adopt the received revision before acknowledging.
    async fn park_received_message(&mut self, message: Message) -> Result<(), RxError> {
        self.adopt_spec_revision(&message);

        match self.handle_rx_ack(&message).await {
            Ok(true) => Ok(()),
            Ok(false) => {
                self.pending_rx.push(message);
                Ok(())
            }
            Err(RxError::HardReset) => Err(RxError::HardReset),
            Err(RxError::Detached) => Err(RxError::Detached),
            Err(_) => {
                trace!("Failed to acknowledge parked message");
                Ok(())
            }
        }
    }

    /// Validate an outgoing message for spec compliance.
    ///
    /// This catches common mistakes when constructing messages:
    /// - unchunked_extended_messages_supported should always be false
    /// - AVS voltage LSB 2 bits should be zero (per PD 3.2 Tab. 6.26)
    ///
    /// Only validates outgoing messages - never called when parsing received data.
    /// Returns an error if validation fails, allowing the caller to handle it appropriately.
    fn validate_outgoing_message(message: &Message) -> Result<(), TxError> {
        if let Some(Payload::Data(message::data::Data::Request(power_source))) = &message.payload {
            use message::data::request::PowerSource;
            match power_source {
                PowerSource::FixedVariableSupply(rdo) => {
                    if rdo.unchunked_extended_messages_supported() {
                        return Err(TxError::UnchunkedExtendedMessagesNotSupported);
                    }
                }
                PowerSource::Pps(rdo) =>
                {
                    #[allow(clippy::collapsible_match)]
                    if rdo.unchunked_extended_messages_supported() {
                        return Err(TxError::UnchunkedExtendedMessagesNotSupported);
                    }
                }
                #[cfg(feature = "epr")]
                PowerSource::EprRequest(epr) => epr::validate_epr_request(epr)?,
                _ => {}
            }
        }
        Ok(())
    }

    /// Adopt the specification revision of a received SOP message (per PD 3.2 Sec. 6.2.1.1.5).
    ///
    /// Cable messages do not change the revision used with the port partner.
    fn adopt_spec_revision(&mut self, message: &Message) {
        if message.sop == Sop::Sop
            && let Ok(revision) = message.header.spec_revision()
        {
            self.default_header = self.default_header.with_spec_revision(revision);
        }
    }

    async fn transmit_inner(&mut self, sop: Sop, buffer: &[u8]) -> Result<(), TxError> {
        loop {
            match self.driver.transmit(sop, buffer).await {
                Ok(_) => return Ok(()),
                Err(DriverTxError::HardReset) => return Err(TxError::HardReset),
                Err(DriverTxError::Detached) => return Err(TxError::Detached),
                Err(DriverTxError::Discarded) => {
                    // Retry transmission.
                }
            }
        }
    }

    /// Stamp the per-SOP `MessageIDCounter` into a header, and clear the port-only bits for cable SOPs.
    fn stamp_header(&self, header: Header, sop: Sop) -> Header {
        let template = self.header_template(sop);
        // Keep the message type, object count and extended flag; take roles and revision from the template.
        const PORT_ONLY_BITS: u16 = (1 << 8) | (1 << 5);
        let header = match sop {
            Sop::Sop => header,
            _ => Header((header.0 & !PORT_ONLY_BITS) | (template.0 & PORT_ONLY_BITS)),
        };
        header.with_message_id(self.counters.of(sop).tx_message.value())
    }

    /// Transmit a message with the SOP set in `message.sop`.
    ///
    /// Extended messages longer than one chunk (26 bytes) are sent in chunks, per PD 3.2 Sec. 6.12.2.1.3.
    ///
    // GoodCrc message transmission is handled separately.
    // See `transmit_good_crc()` instead.
    pub async fn transmit(&mut self, mut message: Message) -> Result<(), ProtocolError> {
        assert_ne!(
            message.header.message_type(),
            MessageType::Control(ControlMessageType::GoodCRC)
        );

        // Validate outgoing message for spec compliance
        Self::validate_outgoing_message(&message)?;

        trace!("Transmit message: {:?}", message);

        let sop = message.sop;
        let mut buffer = Self::get_message_buffer();

        let size = match &message.payload {
            Some(Payload::Extended(extended)) => {
                let mut data = [0u8; MAX_EXTENDED_MSG_LEN];
                let data_size = extended.to_bytes(&mut data);
                if data_size > MAX_EXTENDED_MSG_CHUNK_LEN {
                    return self.transmit_chunked(message.header, sop, &data[..data_size]).await;
                }
                Self::chunk_to_bytes(
                    self.stamp_header(message.header, sop),
                    data_size,
                    0,
                    &data[..data_size],
                    &mut buffer,
                )
            }
            _ => {
                message.header = self.stamp_header(message.header, sop);
                message.to_bytes(&mut buffer)
            }
        };

        self.transmit_frame(sop, &buffer[..size]).await
    }

    /// Serialize one chunk of an extended message: the message header with the chunk's object count, the
    /// extended header and the chunk data, padded to a whole data object (per PD 3.2 Sec. 6.2.1.2).
    ///
    /// Returns the number of bytes written.
    fn chunk_to_bytes(header: Header, data_size: usize, chunk_number: u8, chunk: &[u8], buffer: &mut [u8]) -> usize {
        let num_objects = (EXT_HEADER_SIZE + chunk.len()).div_ceil(4);
        let ext_header = message::extended::ExtendedHeader::new(data_size as u16)
            .with_chunked(true)
            .with_chunk_number(chunk_number);

        let mut offset = header.with_num_objects(num_objects as u8).to_bytes(buffer);
        offset += ext_header.to_bytes(&mut buffer[offset..]);
        buffer[offset..offset + chunk.len()].copy_from_slice(chunk);
        let end = MSG_HEADER_SIZE + num_objects * 4;
        buffer[offset + chunk.len()..end].fill(0);
        end
    }

    /// Send an extended message of more than one chunk, per PD 3.2 Sec. 6.12.2.1.3 (Chunked Tx state machine).
    ///
    /// After each chunk but the last, the receiver must request the next one within tChunkSenderRequest.
    /// If no Chunk Request follows chunk 0, the partner does not do chunking and the message counts as sent
    /// (it answers `Not_Supported`, a cable plug ignores the message).
    async fn transmit_chunked(&mut self, header: Header, sop: Sop, data: &[u8]) -> Result<(), ProtocolError> {
        let MessageType::Extended(message_type) = header.message_type() else {
            unreachable!("only extended messages are chunked");
        };
        let chunk_count = data.len().div_ceil(MAX_EXTENDED_MSG_CHUNK_LEN);

        for chunk_number in 0..chunk_count {
            let start = chunk_number * MAX_EXTENDED_MSG_CHUNK_LEN;
            let end = (start + MAX_EXTENDED_MSG_CHUNK_LEN).min(data.len());

            let mut buffer = Self::get_message_buffer();
            let size = Self::chunk_to_bytes(
                self.stamp_header(header, sop),
                data.len(),
                chunk_number as u8,
                &data[start..end],
                &mut buffer,
            );
            trace!("Transmit chunk {} of {} for {:?}", chunk_number, chunk_count, message_type);
            self.transmit_frame(sop, &buffer[..size]).await?;

            if chunk_number + 1 == chunk_count {
                break;
            }

            match self.wait_for_chunk_request(sop, message_type).await? {
                Some(requested) if requested as usize == chunk_number + 1 => {}
                Some(requested) => {
                    trace!("Chunk {} requested, expected {}", requested, chunk_number + 1);
                    return Err(TxError::ChunkedTransmissionFailed.into());
                }
                None if chunk_number == 0 => {
                    trace!("No chunk request after chunk 0: partner does not support chunking");
                    return Ok(());
                }
                None => return Err(TxError::ChunkedTransmissionFailed.into()),
            }
        }

        Ok(())
    }

    /// Wait for a Chunk Request for an extended message of `message_type` on `sop`.
    ///
    /// Returns the requested chunk number, or `None` when tChunkSenderRequest expired. Any other message aborts
    /// the chunked transmission: it is parked for the policy engine and the transmission fails.
    async fn wait_for_chunk_request(
        &mut self,
        sop: Sop,
        message_type: ExtendedMessageType,
    ) -> Result<Option<u8>, ProtocolError> {
        let timeout_fut = Self::get_timer(TimerType::ChunkSenderRequest);
        let receive_fut = async {
            loop {
                let mut buffer = Self::get_message_buffer();
                let Some(frame) = self.driver_receive(&mut buffer).await? else {
                    continue;
                };
                if frame.len < MSG_HEADER_SIZE {
                    continue;
                }

                // Per PD 3.2 Sec. 6.6.1, a corrupt header is dropped.
                let Ok(header) = Header::from_bytes(&buffer[..MSG_HEADER_SIZE]) else {
                    continue;
                };
                if matches!(
                    header.message_type(),
                    MessageType::Control(ControlMessageType::GoodCRC)
                ) {
                    continue;
                }

                let probe = Message::new(header).with_sop(frame.sop);
                if self.handle_rx_ack(&probe).await? {
                    continue; // Retransmission
                }

                if frame.sop == sop
                    && header.message_type() == MessageType::Extended(message_type)
                    && frame.len >= MSG_HEADER_SIZE + EXT_HEADER_SIZE
                {
                    let ext_header = message::extended::ExtendedHeader::from_bytes(
                        &buffer[MSG_HEADER_SIZE..MSG_HEADER_SIZE + EXT_HEADER_SIZE],
                    );
                    if ext_header.request_chunk() {
                        return Ok(Some(ext_header.chunk_number()));
                    }
                }

                // Per PD 3.2 Sec. 6.12.2.1.3.8: any other message ends the chunked transmission and goes to the
                // receive path. It was acknowledged above.
                if let Ok(message) = Message::from_bytes(&buffer[..frame.len]) {
                    let message = message.with_sop(frame.sop);
                    self.adopt_spec_revision(&message);
                    self.pending_rx.push(message);
                }
                return Err(TxError::ChunkedTransmissionFailed.into());
            }
        };

        match select(timeout_fut, receive_fut).await {
            Either::First(_) => Ok(None),
            Either::Second(result) => result,
        }
    }

    /// Transmit one frame and wait for its GoodCRC, retrying per PD 3.2 Sec. 6.7.2.
    ///
    /// The frame's header must carry the current `MessageIDCounter` of `sop` (see [`Self::stamp_header`]).
    async fn transmit_frame(&mut self, sop: Sop, frame: &[u8]) -> Result<(), ProtocolError> {
        if DRIVER::HAS_AUTO_RETRY {
            // Hardware handles retries and verifies GoodCRC reception.
            // Call driver.transmit() directly (not transmit_inner()) because
            // Discarded here means all hardware retries exhausted — no point
            // retrying in software.
            match self.driver.transmit(sop, frame).await {
                Ok(()) => {
                    self.counters.retry.reset();
                    _ = self.counters.of_mut(sop).tx_message.increment();
                    trace!("Transmit success (hardware retry)");
                    Ok(())
                }
                Err(DriverTxError::HardReset) => Err(TxError::HardReset.into()),
                Err(DriverTxError::Detached) => Err(TxError::Detached.into()),
                Err(DriverTxError::Discarded) => {
                    // Per PD 3.2 Sec. 6.12.2.2.1.9 (`PRL_Tx_Transmission_Error`): increment the `MessageIDCounter` and
                    // inform the Policy Engine of the transmission error, so the partner's stored ID is not reused.
                    _ = self.counters.of_mut(sop).tx_message.increment();
                    Err(ProtocolError::TransmitRetriesExceeded(self.counters.retry.max_value()))
                }
            }
        } else {
            // Software retry loop
            self.counters.retry.reset();

            loop {
                match self.transmit_inner(sop, frame).await {
                    Ok(_) => match self.wait_for_good_crc(sop).await {
                        Ok(()) => {
                            trace!("Transmit success");
                            return Ok(());
                        }
                        Err(RxError::ReceiveTimeout | RxError::AcknowledgeMismatch(_)) => {
                            match self.counters.retry.increment() {
                                Ok(_) => {
                                    // Retry transmission, until the retry counter is exceeded.
                                }
                                Err(CounterError::Exceeded) => {
                                    // Per PD 3.2 Sec. 6.12.2.2.1.9 (PRL_Tx_Transmission_Error): increment the `MessageIDCounter`
                                    // and inform the Policy Engine of the transmission error, so the partner's stored ID is not reused.
                                    let _ = self.counters.of_mut(sop).tx_message.increment();
                                    return Err(ProtocolError::TransmitRetriesExceeded(
                                        self.counters.retry.max_value(),
                                    ));
                                }
                            }
                        }
                        Err(other) => return Err(other.into()),
                    },
                    Err(other) => return Err(other.into()),
                }
            }
        }
    }

    /// Send a GoodCrc message acknowledging `received`, on its SOP.
    ///
    /// Per PD 3.2 Sec. 6.2.1.1.5, the GoodCRC carries the specification revision of the message it acknowledges.
    async fn transmit_good_crc(&mut self, received: &Message) -> Result<(), ProtocolError> {
        let message_id = received.header.message_id();
        trace!("Transmit message GoodCrc for RX message count: {}", message_id);

        let mut template = self.header_template(received.sop);
        if let Ok(revision) = received.header.spec_revision() {
            template = template.with_spec_revision(revision);
        }

        let mut buffer = Self::get_message_buffer();
        let size = Message::new(Header::new_control(
            template,
            Counter::new_from_value(CounterType::MessageId, message_id),
            ControlMessageType::GoodCRC,
        ))
        .to_bytes(&mut buffer);

        Ok(self.transmit_inner(received.sop, &buffer[..size]).await?)
    }

    /// Handle acknowledgement and retransmission detection for a received message.
    ///
    /// Returns `Ok(true)` if this was a retransmission (caller should continue to next message),
    /// `Ok(false)` if this is a new message to process, or `Err` on failure.
    async fn handle_rx_ack(&mut self, message: &Message) -> Result<bool, RxError> {
        let is_good_crc = matches!(
            message.header.message_type(),
            MessageType::Control(ControlMessageType::GoodCRC)
        );

        let is_retransmission = if is_good_crc {
            false
        } else {
            self.update_rx_message_counter(message)
        };

        if !DRIVER::HAS_AUTO_GOOD_CRC && !is_good_crc {
            match self.transmit_good_crc(message).await {
                Ok(()) => {}
                Err(ProtocolError::TxError(TxError::HardReset)) => return Err(RxError::HardReset),
                Err(ProtocolError::TxError(TxError::Detached)) => return Err(RxError::Detached),
                Err(_) => return Err(RxError::UnsupportedMessage),
            }
        }

        Ok(is_retransmission)
    }

    /// Reset chunked message reception state.
    fn reset_chunked_rx(&mut self) {
        self.extended_rx_buffer.clear();
        self.extended_rx_expected = None;
    }

    /// Receive a message, assembling chunked extended messages as needed.
    async fn receive_message_inner(&mut self) -> Result<Message, RxError> {
        loop {
            // Deliver parked messages (during wait for GoodCRC).
            //
            // They were parsed, acknowledged and counted when parked (per 6.7.1.2), so they skip driver receive,
            // chunk assembly and the acknowledgement pipeline. Only post-parse checks are repeated.
            // Update of spec revision is mirrored here.
            if let Some(message) = self.pending_rx.pop() {
                self.adopt_spec_revision(&message);

                match message.header.message_type() {
                    MessageType::Control(ControlMessageType::Reserved)
                    | MessageType::Data(DataMessageType::Reserved) => {
                        trace!("Unsupported message type in header: {:?}", message.header);
                        return Err(RxError::UnsupportedMessage);
                    }
                    MessageType::Control(ControlMessageType::SoftReset) if message.sop == Sop::Sop => {
                        return Err(RxError::SoftReset);
                    }
                    _ => (),
                }

                trace!("Received message {:?}", message);
                return Ok(message);
            }

            let mut buffer = Self::get_message_buffer();

            let Some(frame) = self.driver_receive(&mut buffer).await? else {
                continue;
            };
            let (sop, length) = (frame.sop, frame.len);
            if length < MSG_HEADER_SIZE {
                continue;
            }

            // Parse header early to handle chunking.
            let header = Header::from_bytes(&buffer[..MSG_HEADER_SIZE])?;
            let message_type = header.message_type();

            if matches!(message_type, MessageType::Extended(_)) {
                let ext_header_end = MSG_HEADER_SIZE + EXT_HEADER_SIZE;
                if length < ext_header_end {
                    continue;
                }
                let ext_header =
                    message::extended::ExtendedHeader::from_bytes(&buffer[MSG_HEADER_SIZE..ext_header_end]);
                let payload = &buffer[ext_header_end..length];
                let total_size = ext_header.data_size();
                let chunked = ext_header.chunked();
                let chunk_number = ext_header.chunk_number();
                let msg_type = match message_type {
                    MessageType::Extended(mt) => mt,
                    _ => unreachable!(),
                };

                // Update specification revision, based on the received frame.
                header.spec_revision()?;
                self.adopt_spec_revision(&Message::new(header).with_sop(sop));

                if chunked {
                    trace!(
                        "Received chunked extended message {:?}, chunk {}, size {}",
                        message_type,
                        chunk_number,
                        payload.len()
                    );

                    // Update RX counters and acknowledge.
                    let tmp_message = Message::new(header).with_sop(sop);
                    if self.handle_rx_ack(&tmp_message).await? {
                        continue; // Retransmission
                    }

                    let (expected_total, expected_next) = match self.extended_rx_expected {
                        Some((ty, total, next)) if ty == msg_type => (total, next),
                        _ => (total_size, 0),
                    };

                    // Ensure chunks arrive in order.
                    if expected_next != 0 && chunk_number != expected_next {
                        self.reset_chunked_rx();
                        return Err(RxError::UnsupportedMessage);
                    }

                    if chunk_number == 0 || expected_next == 0 {
                        self.extended_rx_buffer.clear();
                        self.extended_rx_expected = Some((msg_type, total_size, 1));
                    } else {
                        self.extended_rx_expected = Some((msg_type, expected_total, expected_next + 1));
                    }

                    if self.extended_rx_buffer.len() + payload.len() > self.extended_rx_buffer.capacity() {
                        self.reset_chunked_rx();
                        return Err(RxError::UnsupportedMessage);
                    }
                    if self.extended_rx_buffer.extend_from_slice(payload).is_err() {
                        self.reset_chunked_rx();
                        return Err(RxError::UnsupportedMessage);
                    }

                    if self.extended_rx_buffer.len() < total_size as usize {
                        // Need more chunks - send chunk request per PD 3.2 Sec. 6.12.2.1.2.4
                        let next_chunk = self
                            .extended_rx_expected
                            .as_ref()
                            .map(|(_, _, next)| *next)
                            .unwrap_or(1);
                        self.transmit_chunk_request(sop, msg_type, next_chunk).await?;
                        continue;
                    }

                    // All chunks received, parse payload.
                    let ext_payload = &self.extended_rx_buffer[..total_size as usize];
                    let parsed_payload = match msg_type {
                        ExtendedMessageType::ExtendedControl => {
                            Payload::Extended(message::extended::Extended::ExtendedControl(
                                message::extended::extended_control::ExtendedControl::from_bytes(ext_payload),
                            ))
                        }
                        #[cfg(feature = "epr")]
                        ExtendedMessageType::EprSourceCapabilities => {
                            Payload::Extended(message::extended::Extended::EprSourceCapabilities(
                                message::extended::epr::parse_pdos(ext_payload),
                            ))
                        }
                        _ => Payload::Extended(message::extended::Extended::Unknown),
                    };

                    self.extended_rx_expected = None;
                    let mut message = Message::new(header).with_sop(sop);
                    message.payload = Some(parsed_payload);

                    trace!("Received assembled extended message {:?}", message);
                    return Ok(message);
                }
            }

            // Non-extended or unchunked extended messages.
            let message = Message::from_bytes(&buffer[..length])?.with_sop(sop);

            // Update specification revision, based on the received frame.
            self.adopt_spec_revision(&message);

            match message.header.message_type() {
                MessageType::Control(ControlMessageType::Reserved) | MessageType::Data(DataMessageType::Reserved) => {
                    trace!("Unsupported message type in header: {:?}", message.header);
                    return Err(RxError::UnsupportedMessage);
                }
                MessageType::Control(ControlMessageType::SoftReset) if sop.is_cable() => {
                    // A Soft_Reset to a cable plug resets that SOP only (per PD 3.2 Sec. 6.7.1), and goes up as a
                    // message to whoever plays the cable plug.
                    *self.counters.of_mut(sop) = SopCounters::default();
                    match self.handle_rx_ack(&message).await? {
                        true => continue,
                        false => return Ok(message),
                    }
                }
                MessageType::Control(ControlMessageType::SoftReset) => {
                    // Per PD 3.2 Sec. 6.12.2.3.2/.3: reset the `MessageIDCounter` and clear the stored `MessageID` in
                    // `PRL_Rx_Layer_Reset_for_Receive`, then send `GoodCRC` from `PRL_RxSend_GoodCRC`
                    // once reset actions completed.
                    //
                    // Per PD 3.2 Sec. 6.8.1, the receiver resets its `MessageIDCounter` and `RetryCounter`
                    // before the `Accept` response.
                    self.reset();
                    match self.handle_rx_ack(&message).await? {
                        true => continue, // Retransmission of the `Soft_Reset`
                        false => return Err(RxError::SoftReset),
                    }
                }
                _ => (),
            }

            // Handle GoodCRC and retransmissions.
            if self.handle_rx_ack(&message).await? {
                continue; // Retransmission
            }

            trace!("Received message {:?}", message);
            return Ok(message);
        }
    }

    /// Receive a message.
    ///
    /// Waits until a valid message arrives or the receive timeout expires.
    pub async fn receive_message(&mut self) -> Result<Message, ProtocolError> {
        self.receive_message_inner().await.map_err(|err| err.into())
    }

    /// Updates the received message counter.
    ///
    /// If receiving the first message after protocol layer reset, copy its ID.
    /// Otherwise, compare the received ID with the stored ID. If they are equal, this is a retransmission.
    ///
    /// Returns `true`, if this was a retransmission.
    fn update_rx_message_counter(&mut self, rx_message: &Message) -> bool {
        let rx_counter = &mut self.counters.of_mut(rx_message.sop).rx_message;
        match rx_counter.as_mut() {
            None => {
                trace!(
                    "Received first message after protocol layer reset with RX counter value: {}",
                    rx_message.header.message_id()
                );
                *rx_counter = Some(Counter::new_from_value(
                    CounterType::MessageId,
                    rx_message.header.message_id(),
                ));
                false
            }
            Some(counter) => {
                if rx_message.header.message_id() == counter.value() {
                    trace!("Received retransmission of RX counter value: {}", counter.value());
                    true
                } else {
                    counter.set(rx_message.header.message_id());
                    false
                }
            }
        }
    }

    /// Wait until a SOP message of one of the chosen types is received, or a timeout occurs.
    ///
    /// Messages on other SOPs (cable traffic) are acknowledged and dropped: the policy engine waits on its partner.
    pub async fn receive_message_type(
        &mut self,
        message_types: &[MessageType],
        timer_type: TimerType,
    ) -> Result<Message, ProtocolError> {
        // GoodCrc message reception is handled separately.
        // See `wait_for_good_crc()` instead.
        for message_type in message_types {
            assert_ne!(*message_type, MessageType::Control(ControlMessageType::GoodCRC));
        }

        let timeout_fut = Self::get_timer(timer_type);
        let receive_fut = async {
            loop {
                match self.receive_message_inner().await {
                    Ok(message) => {
                        if matches!(
                            message.header.message_type(),
                            MessageType::Control(ControlMessageType::GoodCRC)
                        ) {
                            continue;
                        }
                        if message.sop != Sop::Sop {
                            trace!("Dropping {:?} message while waiting for SOP", message.sop);
                            continue;
                        }
                        return if message_types.contains(&message.header.message_type()) {
                            Ok(message)
                        } else {
                            Err(ProtocolError::UnexpectedMessage)
                        };
                    }
                    Err(RxError::ParseError(err)) => {
                        // Per PD 3.2 Sec. 6.6.1, a corrupt header is dropped.
                        // The sender retries after its `CRCReceiveTimer` expiry.
                        trace!("Dropping frame with parse error: {:?}", err);
                        continue;
                    }
                    Err(other) => return Err(other.into()),
                }
            }
        };

        match select(timeout_fut, receive_fut).await {
            Either::First(_) => Err(RxError::ReceiveTimeout.into()),
            Either::Second(receive_result) => receive_result,
        }
    }

    /// Perform a hard-reset procedure.
    ///
    // Per PD 3.2 Sec. 6.7.1.1
    pub async fn hard_reset(&mut self) -> Result<(), ProtocolError> {
        // Per PD 3.2 Sec. 6.12.2.3.2, pending receive state is cleared on layer reset.
        self.pending_rx.clear();
        // Per PD 3.2 Sec. 6.12.2.4.1 / Tab. 8.58: the `MessageIDCounter`, the stored copy of the `MessageID` and
        // the `RetryCounter` are all reset.
        self.reset();
        self.counters.retry.reset();
        // Stale chunked-assembly state must not survive a layer reset.
        self.reset_chunked_rx();

        loop {
            match self.driver.transmit_hard_reset().await {
                Ok(_) | Err(DriverTxError::HardReset) => break,
                Err(DriverTxError::Detached) => return Err(TxError::Detached.into()),
                Err(DriverTxError::Discarded) => (),
            }
        }

        trace!("Performed hard reset");
        Ok(())
    }

    /// Wait for VBUS to be available.
    pub async fn wait_for_vbus(&mut self) {
        self.driver.wait_for_vbus().await
    }

    /// Transmit a control message of the provided type.
    pub async fn transmit_control_message(&mut self, message_type: ControlMessageType) -> Result<(), ProtocolError> {
        let message = Message::new(Header::new_control(
            self.default_header,
            self.tx_counter(),
            message_type,
        ));

        self.transmit(message).await
    }

    /// Transmit an extended control message of the provided type.
    #[cfg(any(feature = "epr", test))]
    pub async fn transmit_extended_control_message(
        &mut self,
        message_type: ExtendedControlMessageType,
    ) -> Result<(), ProtocolError> {
        // Per PD 3.2 Sec. 6.2.1.1.2: for extended messages, num_objects must be non-zero.
        // ExtendedControl = 2-byte extended header + 2-byte data = 4 bytes = 1 data object.
        let mut message = Message::new(Header::new_extended(
            self.default_header,
            self.tx_counter(),
            ExtendedMessageType::ExtendedControl,
            1,
        ));

        message.payload = Some(Payload::Extended(Extended::ExtendedControl(
            message::extended::extended_control::ExtendedControl::default().with_message_type(message_type),
        )));

        self.transmit(message).await
    }

    /// Transmit a chunk request message per PD 3.2 Sec. 6.12.2.1.2.4.
    ///
    /// A chunk request is an extended message with:
    /// - The same message type as the chunked message being received
    /// - Extended header with: chunked=1, request_chunk=1, chunk_number=requested_chunk, data_size=0
    async fn transmit_chunk_request(
        &mut self,
        sop: Sop,
        message_type: ExtendedMessageType,
        chunk_number: u8,
    ) -> Result<(), RxError> {
        trace!("Transmit chunk request for {:?} chunk {}", message_type, chunk_number);

        // Build extended header for chunk request
        let ext_header = message::extended::ExtendedHeader::default()
            .with_chunked(true)
            .with_request_chunk(true)
            .with_chunk_number(chunk_number);

        // Build message header - num_objects = 1 for the extended header word
        let header = self.stamp_header(
            Header::new_extended(self.default_header, self.tx_counter(), message_type, 1),
            sop,
        );

        // Build message bytes manually
        let mut buffer = Self::get_message_buffer();
        let mut offset = header.to_bytes(&mut buffer);
        offset += ext_header.to_bytes(&mut buffer[offset..]);
        // Pad to 4-byte Data Object boundary per PD 3.2.
        // Extended header is 2 bytes, so add 2 bytes padding to complete the Data Object.
        // Buffer is already zeroed, so just advance offset.
        offset += 2;

        match self.transmit_frame(sop, &buffer[..offset]).await {
            Ok(()) => Ok(()),
            Err(ProtocolError::RxError(error)) => Err(error),
            Err(ProtocolError::TxError(TxError::HardReset)) => Err(RxError::HardReset),
            Err(ProtocolError::TxError(TxError::Detached)) => Err(RxError::Detached),
            Err(_) => Err(RxError::ReceiveTimeout),
        }
    }

    /// Transmit sink capabilities in response to Get_Sink_Cap.
    ///
    /// Per PD 3.2 Sec. 6.4.1.6, sinks respond to Get_Sink_Cap messages
    /// with a Sink_Capabilities message containing PDOs describing what power levels
    /// the sink can operate at.
    pub async fn transmit_sink_capabilities(
        &mut self,
        capabilities: message::data::sink_capabilities::SinkCapabilities,
    ) -> Result<(), ProtocolError> {
        let num_objects = capabilities.num_objects();
        let header = Header::new_data(
            self.default_header,
            self.tx_counter(),
            DataMessageType::SinkCapabilities,
            num_objects,
        );

        self.transmit(Message::new_with_data(header, Data::SinkCapabilities(capabilities)))
            .await
    }

    /// Transmit the device's Source Capabilities
    ///
    /// Could be sent from a Source or Dual Role Device
    pub async fn transmit_source_capabilities(
        &mut self,
        source_capabilities: &SourceCapabilities,
    ) -> Result<(), ProtocolError> {
        // Only sources can send capabilities
        debug_assert!(matches!(self.default_header.port_power_role(), PowerRole::Source));
        // Per PD 3.2 Sec. 8.3.3.3.8: reject SPR capabilities containing EPR PDOs
        #[cfg(feature = "epr")]
        if source_capabilities.has_epr_pdo_in_spr_positions() {
            return Err(ProtocolError::TxError(TxError::HardReset));
        }

        let header = Header::new_data(
            self.default_header,
            self.tx_counter(),
            DataMessageType::SourceCapabilities,
            source_capabilities.0.len() as u8, // Raw cast OK since since len has domain of [0, 8]
        );

        let message = Message::new_with_data(header, Data::SourceCapabilities(source_capabilities.clone()));

        self.transmit(message).await
    }
}

#[repr(transparent)]
#[derive(Debug)]
/// The USB PD Protocol Layer for a `Sink`
pub(crate) struct SinkProtocolLayer<DRIVER: Driver, TIMER: Timer>(pub(crate) ProtocolLayer<DRIVER, TIMER>);

impl<DRIVER: Driver, TIMER: Timer> core::ops::Deref for SinkProtocolLayer<DRIVER, TIMER> {
    type Target = ProtocolLayer<DRIVER, TIMER>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<DRIVER: Driver, TIMER: Timer> core::ops::DerefMut for SinkProtocolLayer<DRIVER, TIMER> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<DRIVER: Driver, TIMER: Timer> SinkProtocolLayer<DRIVER, TIMER> {
    /// Create a new protocol layer from a driver and default header.
    pub fn new(driver: DRIVER, default_header: Header) -> Self {
        Self(ProtocolLayer::new(driver, default_header))
    }

    pub fn deconstruct(self) -> DRIVER {
        self.0.deconstruct()
    }

    /// Wait for the source to provide its capabilities.
    pub async fn wait_for_source_capabilities(&mut self) -> Result<Message, ProtocolError> {
        // Only sinks can await capabilities.
        debug_assert!(matches!(self.default_header.port_power_role(), PowerRole::Sink));

        self.receive_message_type(
            &[
                MessageType::Data(message::header::DataMessageType::SourceCapabilities),
                #[cfg(feature = "epr")]
                MessageType::Extended(ExtendedMessageType::EprSourceCapabilities),
            ],
            TimerType::SinkWaitCap,
        )
        .await
    }

    /// Request a certain power level from the source.
    pub async fn request_power(&mut self, power_source_request: request::PowerSource) -> Result<(), ProtocolError> {
        // Only sinks can request from a supply.
        debug_assert!(matches!(self.default_header.port_power_role(), PowerRole::Sink));

        let message_type = power_source_request.message_type();
        let num_objects = power_source_request.num_objects();
        let header = Header::new_data(self.default_header, self.tx_counter(), message_type, num_objects);

        self.transmit(Message::new_with_data(header, Data::Request(power_source_request)))
            .await
    }
}

#[repr(transparent)]
#[derive(Debug)]
/// The USB PD Protocol Layer for a `Source`
pub(crate) struct SourceProtocolLayer<DRIVER: Driver, TIMER: Timer>(pub(crate) ProtocolLayer<DRIVER, TIMER>);

impl<DRIVER: Driver, TIMER: Timer> core::ops::Deref for SourceProtocolLayer<DRIVER, TIMER> {
    type Target = ProtocolLayer<DRIVER, TIMER>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<DRIVER: Driver, TIMER: Timer> core::ops::DerefMut for SourceProtocolLayer<DRIVER, TIMER> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<DRIVER: Driver, TIMER: Timer> SourceProtocolLayer<DRIVER, TIMER> {
    /// Create a new protocol layer from a driver and default header.
    pub fn new(driver: DRIVER, default_header: Header) -> Self {
        Self(ProtocolLayer::new(driver, default_header))
    }

    pub fn deconstruct(self) -> DRIVER {
        self.0.deconstruct()
    }

    /// Wait for the sink to request a capability with a Request Message.
    pub async fn wait_for_request(&mut self) -> Result<Message, ProtocolError> {
        // Only sources await a sink power request
        debug_assert!(matches!(self.default_header.port_power_role(), PowerRole::Source));

        self.receive_message_type(
            &[MessageType::Data(message::header::DataMessageType::Request)],
            TimerType::SenderResponse,
        )
        .await
    }
}

#[cfg(test)]
mod tests {

    use core::iter::zip;

    use super::ProtocolLayer;
    use super::message::data::Data;
    use super::message::data::source_capabilities::SourceCapabilities;
    use super::message::header::Header;
    use crate::dummy::{
        DUMMY_CAPABILITIES, DummyDriver, DummyTimer, MAX_DATA_MESSAGE_SIZE, get_dummy_source_capabilities,
    };
    use crate::protocol_layer::message::Payload;

    fn get_protocol_layer(
        driver: DummyDriver<MAX_DATA_MESSAGE_SIZE>,
    ) -> ProtocolLayer<DummyDriver<MAX_DATA_MESSAGE_SIZE>, DummyTimer> {
        ProtocolLayer::new(
            driver,
            Header::new_template(
                crate::DataRole::Ufp,
                crate::PowerRole::Sink,
                super::message::header::SpecificationRevision::R3_X,
            ),
        )
    }

    #[tokio::test]
    async fn test_it() {
        let mut protocol_layer = get_protocol_layer(DummyDriver::new());

        protocol_layer.driver.inject_received_data(&DUMMY_CAPABILITIES);
        let message = protocol_layer.receive_message().await.unwrap();

        if let Some(Payload::Data(Data::SourceCapabilities(SourceCapabilities(caps)))) = message.payload {
            for (cap, dummy_cap) in zip(caps, get_dummy_source_capabilities()) {
                assert_eq!(cap, dummy_cap);
            }
        } else {
            panic!()
        }
    }

    /// Build a `GoodCRC` frame acknowledging `message_id` (per PD 3.2 Sec. 6.3.1).
    fn good_crc_bytes(message_id: u8) -> [u8; 2] {
        // Header: Control `GoodCRC` (raw type 1), num_objects = 0, spec rev 3.x,
        // power role sink (0), data role UFP (0). Little-endian on the wire.
        let header = 0x0001u16 | ((message_id as u16) << 9) | (0b10 << 6);
        [(header & 0xFF) as u8, (header >> 8) as u8]
    }

    /// Build a `Source_Capabilities` frame with the fixed 5 V / 3 A PDO.
    fn source_caps_bytes(message_id: u8) -> [u8; 6] {
        // Header: Data `Source_Capabilities` (raw type 1), num_objects = 1,
        // spec rev 3.x, power role sink (0), data role UFP (0), followed by one
        // PDO from DUMMY_CAPABILITIES. Little-endian on the wire.
        let header = 0x1000u16 | ((message_id as u16) << 9) | (0b10 << 6) | 0b0_0001;
        let mut buffer = [0u8; 6];
        buffer[0] = (header & 0xFF) as u8;
        buffer[1] = (header >> 8) as u8;
        buffer[2..6].copy_from_slice(&DUMMY_CAPABILITIES[2..6]);
        buffer
    }

    fn transmit_get_source_cap_message() -> super::message::Message {
        super::message::Message::new(Header::new_control(
            Header::new_template(
                crate::DataRole::Ufp,
                crate::PowerRole::Sink,
                super::message::header::SpecificationRevision::R3_X,
            ),
            crate::counters::Counter::new(crate::counters::CounterType::MessageId),
            super::message::header::ControlMessageType::GetSourceCap,
        ))
    }

    /// Build a serialized single-chunk Extended_Control (`EPR_KeepAlive_Ack`) frame from the partner,
    /// exercising the extended-message parking path during a `GoodCRC` wait.
    fn extended_control_bytes(message_id: u8) -> [u8; 8] {
        use byteorder::{ByteOrder, LittleEndian};

        use crate::protocol_layer::message::extended::ExtendedHeader;
        use crate::protocol_layer::message::extended::extended_control::{ExtendedControl, ExtendedControlMessageType};

        // Header: Extended Extended_Control (raw type 16), spec rev 3.x.
        let header = Header::new_extended(
            Header::new_template(
                crate::DataRole::Dfp,
                crate::PowerRole::Source,
                super::message::header::SpecificationRevision::R3_X,
            ),
            crate::counters::Counter::new_from_value(crate::counters::CounterType::MessageId, message_id),
            super::message::header::ExtendedMessageType::ExtendedControl,
            1,
        );

        // Extended header: data size 2, single chunk 0 (fits in one chunk).
        let ext_header = ExtendedHeader::new(2).with_chunked(true).with_chunk_number(0);
        let control = ExtendedControl::default().with_message_type(ExtendedControlMessageType::EprKeepAliveAck);

        let mut buffer = [0u8; 8];
        let mut offset = header.to_bytes(&mut buffer);
        offset += ext_header.to_bytes(&mut buffer[offset..]);
        LittleEndian::write_u16(&mut buffer[offset..], control.0);
        buffer
    }

    #[tokio::test]
    async fn test_non_goodcrc_during_goodcrc_wait_is_parked_and_acked() {
        let mut protocol_layer = get_protocol_layer(DummyDriver::new());

        protocol_layer.driver.inject_received_data(&source_caps_bytes(7));
        protocol_layer.driver.inject_received_data(&good_crc_bytes(0));

        let result = protocol_layer.transmit(transmit_get_source_cap_message()).await;
        result.unwrap();

        // The only transmitted frames are our own Get_Source_Cap and the
        // GoodCRC acking the partner's Source_Capabilities; no retransmission.
        let own = protocol_layer.driver.probe_transmitted_data();
        let own = super::message::Message::from_bytes(&own).unwrap();
        assert_eq!(
            own.header.message_type(),
            super::message::header::MessageType::Control(super::message::header::ControlMessageType::GetSourceCap)
        );
        let good_crc = protocol_layer.driver.probe_transmitted_data();
        let ack = super::message::Message::from_bytes(&good_crc).unwrap();
        assert_eq!(
            ack.header.message_type(),
            super::message::header::MessageType::Control(super::message::header::ControlMessageType::GoodCRC)
        );
        assert_eq!(ack.header.message_id(), 7);
        assert!(!protocol_layer.driver.has_transmitted_data());

        // The parked Source_Capabilities is delivered intact afterwards.
        let message = protocol_layer.receive_message().await.unwrap();
        assert_eq!(
            message.header.message_type(),
            super::message::header::MessageType::Data(super::message::header::DataMessageType::SourceCapabilities)
        );
        assert_eq!(message.header.message_id(), 7);
        assert!(matches!(
            message.payload,
            Some(Payload::Data(Data::SourceCapabilities(_)))
        ));
    }

    #[tokio::test]
    async fn test_extended_message_during_goodcrc_wait_is_parked_and_acked() {
        let mut protocol_layer = get_protocol_layer(DummyDriver::new());

        protocol_layer.driver.inject_received_data(&extended_control_bytes(3));
        protocol_layer.driver.inject_received_data(&good_crc_bytes(0));

        let result = protocol_layer.transmit(transmit_get_source_cap_message()).await;
        result.unwrap();

        // Transmitted frames: our own Get_Source_Cap, then the GoodCRC
        // acknowledging the partner's Extended_Control; no retransmission.
        let own = protocol_layer.driver.probe_transmitted_data();
        assert_eq!(
            super::message::Message::from_bytes(&own).unwrap().header.message_type(),
            super::message::header::MessageType::Control(super::message::header::ControlMessageType::GetSourceCap)
        );
        let good_crc = protocol_layer.driver.probe_transmitted_data();
        let ack = super::message::Message::from_bytes(&good_crc).unwrap();
        assert_eq!(
            ack.header.message_type(),
            super::message::header::MessageType::Control(super::message::header::ControlMessageType::GoodCRC)
        );
        assert_eq!(ack.header.message_id(), 3);
        assert!(!protocol_layer.driver.has_transmitted_data());

        // The parked Extended_Control is delivered intact afterwards.
        let message = protocol_layer.receive_message().await.unwrap();
        assert_eq!(message.header.message_id(), 3);
        assert_eq!(
            message.header.message_type(),
            super::message::header::MessageType::Extended(super::message::header::ExtendedMessageType::ExtendedControl)
        );
        assert!(matches!(
            message.payload,
            Some(super::message::Payload::Extended(
                super::message::extended::Extended::ExtendedControl(_)
            ))
        ));
    }

    #[tokio::test]
    async fn test_mismatched_goodcrc_retries() {
        let mut protocol_layer = get_protocol_layer(DummyDriver::new());

        // Wrong acknowledgment followed by the correct one (original message id 0).
        protocol_layer.driver.inject_received_data(&good_crc_bytes(5));
        protocol_layer.driver.inject_received_data(&good_crc_bytes(0));

        let result = protocol_layer.transmit(transmit_get_source_cap_message()).await;
        result.unwrap();

        // The original frame and one retry share the same MessageID.
        let first = protocol_layer.driver.probe_transmitted_data();
        let second = protocol_layer.driver.probe_transmitted_data();
        assert_eq!(first, second);
        let message = super::message::Message::from_bytes(&first).unwrap();
        assert_eq!(message.header.message_id(), 0);
        assert!(!protocol_layer.driver.has_transmitted_data());
    }

    #[tokio::test]
    async fn test_two_messages_parked_in_order() {
        let mut protocol_layer = get_protocol_layer(DummyDriver::new());

        protocol_layer.driver.inject_received_data(&source_caps_bytes(0));
        protocol_layer.driver.inject_received_data(&source_caps_bytes(1));
        protocol_layer.driver.inject_received_data(&good_crc_bytes(0));

        let result = protocol_layer.transmit(transmit_get_source_cap_message()).await;
        result.unwrap();

        // Transmitted frames: our own Get_Source_Cap, then one GoodCRC per
        // partner message, in arrival order.
        let own = protocol_layer.driver.probe_transmitted_data();
        assert_eq!(
            super::message::Message::from_bytes(&own).unwrap().header.message_type(),
            super::message::header::MessageType::Control(super::message::header::ControlMessageType::GetSourceCap)
        );
        let good_crc = protocol_layer.driver.probe_transmitted_data();
        assert_eq!(
            super::message::Message::from_bytes(&good_crc)
                .unwrap()
                .header
                .message_id(),
            0
        );
        let good_crc = protocol_layer.driver.probe_transmitted_data();
        assert_eq!(
            super::message::Message::from_bytes(&good_crc)
                .unwrap()
                .header
                .message_id(),
            1
        );
        assert!(!protocol_layer.driver.has_transmitted_data());

        // Both parked messages are delivered in order with matching ids.
        let first = protocol_layer.receive_message().await.unwrap();
        let second = protocol_layer.receive_message().await.unwrap();
        assert_eq!(first.header.message_id(), 0);
        assert_eq!(second.header.message_id(), 1);
    }

    #[test]
    fn test_pending_rx_overflow_drops_oldest() {
        let mut pending = super::PendingRx::<{ super::PENDING_RX_CAPACITY }>::default();

        // Push messages with MessageIDs 1, 2, 3; capacity is 2, so the second
        // push overflows and drops the MessageID 1 message.
        for message_id in 1..4u16 {
            let header = super::message::header::Header((message_id << 9) | 0x0001);
            pending.push(super::message::Message::new(header));
        }

        assert_eq!(2, pending.queue.len());
        assert_eq!(Some(2), pending.pop().map(|message| message.header.message_id()));
        assert_eq!(Some(3), pending.pop().map(|message| message.header.message_id()));
        assert!(pending.queue.is_empty());
    }

    /// Build a frame for any SOP: a control message `raw_type` with `message_id`, from a sink UFP at rev 3.x.
    fn control_bytes(raw_type: u16, message_id: u8) -> [u8; 2] {
        let header = raw_type | ((message_id as u16) << 9) | (0b10 << 6);
        [(header & 0xFF) as u8, (header >> 8) as u8]
    }

    #[tokio::test]
    async fn test_sop_prime_has_its_own_message_ids() {
        use usbpd_traits::Sop;

        use super::message::header::{ControlMessageType, MessageType};

        let mut protocol_layer = get_protocol_layer(DummyDriver::new());

        // A SOP and a SOP' message with the same MessageID: neither is a retransmission of the other.
        protocol_layer.driver.inject_received_data(&source_caps_bytes(3));
        protocol_layer
            .driver
            .inject_received_sop_data(Sop::SopPrime, &control_bytes(0b0_0011, 3)); // Accept
        // A SOP' retransmission is dropped.
        protocol_layer
            .driver
            .inject_received_sop_data(Sop::SopPrime, &control_bytes(0b0_0011, 3));
        protocol_layer
            .driver
            .inject_received_sop_data(Sop::SopPrime, &control_bytes(0b0_0011, 4));

        let first = protocol_layer.receive_message().await.unwrap();
        assert_eq!(first.sop, Sop::Sop);
        let second = protocol_layer.receive_message().await.unwrap();
        assert_eq!(second.sop, Sop::SopPrime);
        assert_eq!(second.header.message_id(), 3);
        let third = protocol_layer.receive_message().await.unwrap();
        assert_eq!(third.sop, Sop::SopPrime);
        assert_eq!(third.header.message_id(), 4);

        // Every frame, the retransmission too, was acknowledged on its own SOP.
        let expected = [(Sop::Sop, 3), (Sop::SopPrime, 3), (Sop::SopPrime, 3), (Sop::SopPrime, 4)];
        for (sop, message_id) in expected {
            let (tx_sop, frame) = protocol_layer.driver.probe_transmitted_sop_data();
            let ack = super::message::Message::from_bytes(&frame).unwrap();
            assert_eq!(tx_sop, sop);
            assert_eq!(ack.header.message_type(), MessageType::Control(ControlMessageType::GoodCRC));
            assert_eq!(ack.header.message_id(), message_id);
        }
        assert!(!protocol_layer.driver.has_transmitted_data());
    }

    #[tokio::test]
    async fn test_sop_prime_transmit_uses_cable_header_and_counter() {
        use usbpd_traits::Sop;

        let mut protocol_layer = ProtocolLayer::<_, DummyTimer>::new(
            DummyDriver::<MAX_DATA_MESSAGE_SIZE>::new(),
            Header::new_template(
                crate::DataRole::Dfp,
                crate::PowerRole::Source,
                super::message::header::SpecificationRevision::R3_X,
            ),
        );

        // A SOP GoodCRC does not acknowledge a SOP' transmission; the SOP' one does.
        protocol_layer.driver.inject_received_data(&good_crc_bytes(0));
        protocol_layer
            .driver
            .inject_received_sop_data(Sop::SopPrime, &good_crc_bytes(0));
        protocol_layer
            .transmit(transmit_get_source_cap_message().with_sop(Sop::SopPrime))
            .await
            .unwrap();

        let (sop, frame) = protocol_layer.driver.probe_transmitted_sop_data();
        assert_eq!(sop, Sop::SopPrime);
        let header = super::message::Message::from_bytes(&frame).unwrap().header;
        // Cable Plug bit (8) and the reserved bit 5 are clear for a port's SOP' message.
        assert_eq!(header.0 & ((1 << 8) | (1 << 5)), 0);
        assert_eq!(header.message_id(), 0);
        assert!(!protocol_layer.driver.has_transmitted_data());

        // The SOP' counter moved on, the SOP one did not.
        assert_eq!(protocol_layer.counters.of(Sop::SopPrime).tx_message.value(), 1);
        assert_eq!(protocol_layer.counters.of(Sop::Sop).tx_message.value(), 0);

        // A SOP transmission keeps the port roles.
        protocol_layer.driver.inject_received_data(&good_crc_bytes(0));
        let message = super::message::Message::new(Header::new_control(
            *protocol_layer.header(),
            protocol_layer.tx_counter(),
            super::message::header::ControlMessageType::GetSinkCap,
        ));
        protocol_layer.transmit(message).await.unwrap();
        let (sop, frame) = protocol_layer.driver.probe_transmitted_sop_data();
        assert_eq!(sop, Sop::Sop);
        let header = super::message::Message::from_bytes(&frame).unwrap().header;
        assert_eq!(header.0 & ((1 << 8) | (1 << 5)), (1 << 8) | (1 << 5));
    }

    #[tokio::test]
    async fn test_cable_reset_resets_cable_ids_only() {
        use usbpd_traits::{DriverRxError, Sop};

        let mut protocol_layer = get_protocol_layer(DummyDriver::new());
        protocol_layer
            .driver
            .inject_received_sop_data(Sop::SopPrime, &control_bytes(0b0_0011, 5));
        protocol_layer.driver.inject_receive_error(DriverRxError::CableReset);
        // Same ID after the Cable Reset: a new message, not a retransmission.
        protocol_layer
            .driver
            .inject_received_sop_data(Sop::SopPrime, &control_bytes(0b0_0011, 5));

        assert_eq!(protocol_layer.receive_message().await.unwrap().header.message_id(), 5);
        assert_eq!(protocol_layer.receive_message().await.unwrap().header.message_id(), 5);
    }

    #[tokio::test]
    async fn test_detach_is_reported() {
        use usbpd_traits::DriverRxError;

        use super::{ProtocolError, RxError};

        let mut protocol_layer = get_protocol_layer(DummyDriver::new());
        protocol_layer.driver.inject_receive_error(DriverRxError::Detached);
        assert!(matches!(
            protocol_layer.receive_message().await,
            Err(ProtocolError::RxError(RxError::Detached))
        ));

        // Also while waiting for GoodCRC.
        protocol_layer.driver.inject_receive_error(DriverRxError::Detached);
        assert!(matches!(
            protocol_layer.transmit(transmit_get_source_cap_message()).await,
            Err(ProtocolError::RxError(RxError::Detached))
        ));
    }

    #[test]
    fn test_chunk_to_bytes_pads_to_data_objects() {
        let header = Header::new_template(
            crate::DataRole::Ufp,
            crate::PowerRole::Sink,
            super::message::header::SpecificationRevision::R3_X,
        );
        let mut buffer = [0xFFu8; 32];
        let size = ProtocolLayer::<DummyDriver<MAX_DATA_MESSAGE_SIZE>, DummyTimer>::chunk_to_bytes(
            header,
            30,
            1,
            &[1, 2, 3, 4],
            &mut buffer,
        );
        // Header (2) + extended header (2) + 4 data bytes, padded to 2 data objects.
        assert_eq!(size, 10);
        assert_eq!(Header(u16::from_le_bytes([buffer[0], buffer[1]])).num_objects(), 2);
        assert_eq!(&buffer[4..10], &[1, 2, 3, 4, 0, 0]);
    }

    /// Chunk request from the sink for `chunk_number` of an `EPR_Source_Capabilities`.
    #[cfg(feature = "epr")]
    fn epr_caps_chunk_request_bytes(message_id: u8, chunk_number: u8) -> [u8; 6] {
        use crate::protocol_layer::message::extended::ExtendedHeader;
        use crate::protocol_layer::message::header::ExtendedMessageType;

        let header = Header::new_extended(
            Header::new_template(
                crate::DataRole::Ufp,
                crate::PowerRole::Sink,
                super::message::header::SpecificationRevision::R3_X,
            ),
            crate::counters::Counter::new_from_value(crate::counters::CounterType::MessageId, message_id),
            ExtendedMessageType::EprSourceCapabilities,
            1,
        );
        let ext_header = ExtendedHeader::new(0)
            .with_chunked(true)
            .with_request_chunk(true)
            .with_chunk_number(chunk_number);
        let mut buffer = [0u8; 6];
        let offset = header.to_bytes(&mut buffer);
        ext_header.to_bytes(&mut buffer[offset..]);
        buffer
    }

    /// The 10 PDOs of the captured chunked `EPR_Source_Capabilities` fixture.
    #[cfg(feature = "epr")]
    fn captured_epr_source_capabilities() -> SourceCapabilities {
        use crate::dummy::{DUMMY_EPR_SOURCE_CAPS_CHUNK_0, DUMMY_EPR_SOURCE_CAPS_CHUNK_1};
        use crate::protocol_layer::message::Message;
        use crate::protocol_layer::message::extended::Extended;
        use crate::protocol_layer::message::extended::chunked::{ChunkResult, ChunkedMessageAssembler};
        use crate::protocol_layer::message::header::ExtendedMessageType;

        let mut assembler = ChunkedMessageAssembler::new();
        for chunk in [&DUMMY_EPR_SOURCE_CAPS_CHUNK_0[..], &DUMMY_EPR_SOURCE_CAPS_CHUNK_1[..]] {
            let (header, ext_header, data) = Message::parse_extended_chunk(chunk).unwrap();
            if let ChunkResult::Complete(data) = assembler.process_chunk(header, ext_header, data).unwrap() {
                let Extended::EprSourceCapabilities(pdos) =
                    Message::parse_extended_payload(ExtendedMessageType::EprSourceCapabilities, &data)
                else {
                    panic!("not EPR source capabilities");
                };
                return SourceCapabilities(pdos.into_iter().collect());
            }
        }
        panic!("fixture did not assemble");
    }

    #[cfg(feature = "epr")]
    fn get_source_protocol_layer() -> ProtocolLayer<DummyDriver<MAX_DATA_MESSAGE_SIZE>, DummyTimer> {
        ProtocolLayer::new(
            DummyDriver::new(),
            Header::new_template(
                crate::DataRole::Dfp,
                crate::PowerRole::Source,
                super::message::header::SpecificationRevision::R3_X,
            ),
        )
    }

    /// Sending the captured 40-byte EPR caps reproduces both captured chunks byte for byte.
    #[cfg(feature = "epr")]
    #[tokio::test]
    async fn test_chunked_transmit_matches_capture() {
        use usbpd_traits::Sop;

        use crate::dummy::{DUMMY_EPR_SOURCE_CAPS_CHUNK_0, DUMMY_EPR_SOURCE_CAPS_CHUNK_1};

        let caps = captured_epr_source_capabilities();
        assert_eq!(caps.0.len(), 10);

        let mut protocol_layer = get_source_protocol_layer();
        // The capture has MessageIDs 6 and 7.
        protocol_layer.counters.of_mut(Sop::Sop).tx_message =
            crate::counters::Counter::new_from_value(crate::counters::CounterType::MessageId, 6);

        protocol_layer.driver.inject_received_data(&good_crc_bytes(6));
        protocol_layer
            .driver
            .inject_received_data(&epr_caps_chunk_request_bytes(2, 1));
        protocol_layer.driver.inject_received_data(&good_crc_bytes(7));

        protocol_layer.transmit_epr_source_capabilities(&caps).await.unwrap();

        let chunk_0 = protocol_layer.driver.probe_transmitted_data();
        assert_eq!(&chunk_0[..], &DUMMY_EPR_SOURCE_CAPS_CHUNK_0[..]);
        // GoodCRC for the chunk request.
        let ack = protocol_layer.driver.probe_transmitted_data();
        assert_eq!(super::message::Message::from_bytes(&ack).unwrap().header.message_id(), 2);
        let chunk_1 = protocol_layer.driver.probe_transmitted_data();
        assert_eq!(&chunk_1[..], &DUMMY_EPR_SOURCE_CAPS_CHUNK_1[..]);
        assert!(!protocol_layer.driver.has_transmitted_data());
        assert_eq!(protocol_layer.counters.of(Sop::Sop).tx_message.value(), 0);
    }

    /// Without a chunk request after chunk 0, the message counts as sent (the partner does not chunk).
    #[cfg(feature = "epr")]
    #[tokio::test]
    async fn test_chunked_transmit_without_chunk_request() {
        let caps = captured_epr_source_capabilities();
        let mut protocol_layer = get_source_protocol_layer();
        protocol_layer.driver.inject_received_data(&good_crc_bytes(0));

        protocol_layer.transmit_epr_source_capabilities(&caps).await.unwrap();
        protocol_layer.driver.probe_transmitted_data();
        assert!(!protocol_layer.driver.has_transmitted_data());
    }

    /// A wrong chunk number fails the transmission.
    #[cfg(feature = "epr")]
    #[tokio::test]
    async fn test_chunked_transmit_wrong_chunk_request() {
        use super::{ProtocolError, TxError};

        let caps = captured_epr_source_capabilities();
        let mut protocol_layer = get_source_protocol_layer();
        protocol_layer.driver.inject_received_data(&good_crc_bytes(0));
        protocol_layer
            .driver
            .inject_received_data(&epr_caps_chunk_request_bytes(0, 3));

        assert!(matches!(
            protocol_layer.transmit_epr_source_capabilities(&caps).await,
            Err(ProtocolError::TxError(TxError::ChunkedTransmissionFailed))
        ));
    }

    /// Another message in place of the chunk request aborts the transmission and is delivered afterwards.
    #[cfg(feature = "epr")]
    #[tokio::test]
    async fn test_chunked_transmit_interrupted_by_message() {
        use super::{ProtocolError, TxError};

        let caps = captured_epr_source_capabilities();
        let mut protocol_layer = get_source_protocol_layer();
        protocol_layer.driver.inject_received_data(&good_crc_bytes(0));
        protocol_layer.driver.inject_received_data(&control_bytes(0b0_0111, 4)); // Get_Source_Cap

        assert!(matches!(
            protocol_layer.transmit_epr_source_capabilities(&caps).await,
            Err(ProtocolError::TxError(TxError::ChunkedTransmissionFailed))
        ));
        let message = protocol_layer.receive_message().await.unwrap();
        assert_eq!(message.header.message_id(), 4);
    }
}
