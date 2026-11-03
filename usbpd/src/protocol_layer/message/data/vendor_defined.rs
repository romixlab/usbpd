//! Definitions of vendor defined data message content, per PD 3.2 Sec. 6.4.4.
//!
//! Every conversion from raw bits is total: reserved or unknown values decode to a `Reserved` or `Other`
//! variant that keeps the raw value, so a malformed VDM from the port partner can never panic.
use byteorder::{ByteOrder, LittleEndian};
use heapless::Vec;
use proc_bitfield::bitfield;

/// Size of a VDM header or VDO in bytes.
const VDO_SIZE: usize = core::mem::size_of::<u32>();

/// Maximum number of VDOs that follow the VDM header (7 data objects in total, per PD 3.2 Sec. 6.4.4).
pub const MAX_VDOS: usize = 6;

/// The PD standard ID (PD SID), used for Discover Identity/SVIDs and other generic structured VDMs.
pub const PD_SID: u16 = 0xFF00;

/// The DisplayPort standard ID.
pub const DISPLAYPORT_SID: u16 = 0xFF01;

/// A Vendor_Defined message: a VDM header and up to six VDOs.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct VendorDefined {
    /// The VDM header, structured or unstructured.
    pub header: VdmHeader,
    /// The VDOs that follow the header.
    pub vdos: Vec<u32, MAX_VDOS>,
}

impl VendorDefined {
    /// Create a VDM from a header and VDOs. VDOs beyond [`MAX_VDOS`] are dropped.
    pub fn new(header: VdmHeader, vdos: &[u32]) -> Self {
        Self {
            header,
            vdos: vdos.iter().take(MAX_VDOS).copied().collect(),
        }
    }

    /// Number of data objects in the message (the header plus the VDOs).
    pub fn num_objects(&self) -> u8 {
        1 + self.vdos.len() as u8
    }

    /// Parse a VDM from a data message payload. `num_objects` comes from the message header.
    ///
    /// Returns `None` if the payload holds no VDM header.
    pub fn parse(payload: &[u8], num_objects: usize) -> Option<Self> {
        if payload.len() < VDO_SIZE || num_objects == 0 {
            return None;
        }

        let header = VdmHeader::from(LittleEndian::read_u32(&payload[..VDO_SIZE]));
        let vdos = payload[VDO_SIZE..]
            .as_chunks::<VDO_SIZE>()
            .0
            .iter()
            .take((num_objects - 1).min(MAX_VDOS))
            .map(|buf| LittleEndian::read_u32(buf))
            .collect();

        Some(Self { header, vdos })
    }

    /// Serialize the VDM to a slice, returning the number of written bytes.
    pub fn to_bytes(&self, buf: &mut [u8]) -> usize {
        LittleEndian::write_u32(buf, self.header.into());
        let mut offset = VDO_SIZE;
        for vdo in &self.vdos {
            LittleEndian::write_u32(&mut buf[offset..], *vdo);
            offset += VDO_SIZE;
        }
        offset
    }

    /// The structured VDM header, if this is a structured VDM.
    pub fn structured(&self) -> Option<VdmHeaderStructured> {
        match self.header {
            VdmHeader::Structured(header) => Some(header),
            VdmHeader::Unstructured(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum VendorDataObject {
    VdmHeader(VdmHeader),
    IDHeader(VdmIdentityHeader),
    CertStat(CertStatVDO),
    Product(ProductVDO),
    UFPType(UFPTypeVDO),
}

impl VendorDataObject {
    pub fn to_bytes(self, buf: &mut [u8]) {
        match self {
            VendorDataObject::VdmHeader(header) => header.to_bytes(buf),
            VendorDataObject::IDHeader(header) => header.to_bytes(buf),
            VendorDataObject::CertStat(header) => header.to_bytes(buf),
            VendorDataObject::Product(header) => header.to_bytes(buf),
            VendorDataObject::UFPType(header) => header.to_bytes(buf),
        }
    }
}

impl From<VendorDataObject> for u32 {
    fn from(value: VendorDataObject) -> Self {
        match value {
            VendorDataObject::VdmHeader(header) => header.into(),
            VendorDataObject::IDHeader(header) => header.into(),
            VendorDataObject::CertStat(header) => header.into(),
            VendorDataObject::Product(header) => header.into(),
            VendorDataObject::UFPType(header) => header.into(),
        }
    }
}

/// Structured VDM command type, per PD 3.2 Tab. 6.29. All four values of the 2-bit field are defined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum VdmCommandType {
    InitiatorREQ,
    ResponderACK,
    ResponderNAK,
    ResponderBSY,
}

impl From<VdmCommandType> for u8 {
    fn from(value: VdmCommandType) -> Self {
        match value {
            VdmCommandType::InitiatorREQ => 0,
            VdmCommandType::ResponderACK => 1,
            VdmCommandType::ResponderNAK => 2,
            VdmCommandType::ResponderBSY => 3,
        }
    }
}

impl From<u8> for VdmCommandType {
    fn from(value: u8) -> Self {
        match value & 0b11 {
            0 => VdmCommandType::InitiatorREQ,
            1 => VdmCommandType::ResponderACK,
            2 => VdmCommandType::ResponderNAK,
            _ => VdmCommandType::ResponderBSY,
        }
    }
}

/// Structured VDM command, per PD 3.2 Tab. 6.29.
///
/// Commands 16..=31 are SVID specific; `DisplayPortStatus` and `DisplayPortConfig` are the DisplayPort ones.
/// Everything else decodes to `Other` with the raw 5-bit value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum VdmCommand {
    DiscoverIdentity,
    DiscoverSVIDS,
    DiscoverModes,
    EnterMode,
    ExitMode,
    Attention,
    DisplayPortStatus,
    DisplayPortConfig,
    /// A reserved or SVID-specific command not listed above.
    Other(u8),
}

impl From<VdmCommand> for u8 {
    fn from(value: VdmCommand) -> Self {
        match value {
            VdmCommand::DiscoverIdentity => 0x1,
            VdmCommand::DiscoverSVIDS => 0x2,
            VdmCommand::DiscoverModes => 0x3,
            VdmCommand::EnterMode => 0x4,
            VdmCommand::ExitMode => 0x5,
            VdmCommand::Attention => 0x6,
            VdmCommand::DisplayPortStatus => 0x10,
            VdmCommand::DisplayPortConfig => 0x11,
            VdmCommand::Other(raw) => raw & 0x1F,
        }
    }
}

impl From<u8> for VdmCommand {
    fn from(value: u8) -> Self {
        match value {
            0x01 => VdmCommand::DiscoverIdentity,
            0x02 => VdmCommand::DiscoverSVIDS,
            0x03 => VdmCommand::DiscoverModes,
            0x04 => VdmCommand::EnterMode,
            0x05 => VdmCommand::ExitMode,
            0x06 => VdmCommand::Attention,
            0x10 => VdmCommand::DisplayPortStatus,
            0x11 => VdmCommand::DisplayPortConfig,
            other => VdmCommand::Other(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum VdmType {
    Unstructured,
    Structured,
}

impl From<VdmType> for bool {
    fn from(value: VdmType) -> Self {
        match value {
            VdmType::Unstructured => false,
            VdmType::Structured => true,
        }
    }
}

impl From<bool> for VdmType {
    fn from(value: bool) -> Self {
        match value {
            true => VdmType::Structured,
            false => VdmType::Unstructured,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum VdmHeader {
    Structured(VdmHeaderStructured),
    Unstructured(VdmHeaderUnstructured),
}

impl VdmHeader {
    pub fn to_bytes(self, buf: &mut [u8]) {
        match self {
            VdmHeader::Structured(header) => header.to_bytes(buf),
            VdmHeader::Unstructured(header) => header.to_bytes(buf),
        }
    }

    /// The SVID (structured) or vendor ID (unstructured) of the VDM.
    pub fn standard_or_vid(self) -> u16 {
        match self {
            VdmHeader::Structured(header) => header.standard_or_vid(),
            VdmHeader::Unstructured(header) => header.standard_or_vid(),
        }
    }
}

impl From<VdmHeader> for u32 {
    fn from(value: VdmHeader) -> Self {
        match value {
            VdmHeader::Structured(header) => header.into(),
            VdmHeader::Unstructured(header) => header.into(),
        }
    }
}

impl From<u32> for VdmHeader {
    fn from(value: u32) -> Self {
        let header = VdmHeaderRaw(value);
        match header.vdm_type() {
            VdmType::Structured => VdmHeader::Structured(VdmHeaderStructured(value)),
            VdmType::Unstructured => VdmHeader::Unstructured(VdmHeaderUnstructured(value)),
        }
    }
}

bitfield! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    pub struct VdmHeaderRaw(pub u32): FromStorage, IntoStorage {
        /// VDM Standard or Vendor ID
        pub standard_or_vid: u16 @ 16..=31,
        /// VDM Type (Unstructured/Structured)
        pub vdm_type: bool [VdmType] @ 15,
    }
}

impl VdmHeaderRaw {
    pub fn to_bytes(self, buf: &mut [u8]) {
        LittleEndian::write_u32(buf, self.0);
    }
}

bitfield! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    pub struct VdmHeaderStructured(pub u32): FromStorage, IntoStorage {
        /// VDM Standard or Vendor ID
        pub standard_or_vid: u16 @ 16..=31,
        /// VDM Type (Unstructured/Structured)
        pub vdm_type: bool [VdmType] @ 15,
        /// Structured VDM version, major
        pub vdm_version_major: u8 @ 13..=14,
        /// Structured VDM version, minor
        pub vdm_version_minor: u8 @ 11..=12,
        /// Object Position
        pub object_position: u8 @ 8..=10,
        /// Command Type
        pub command_type: u8 [VdmCommandType] @ 6..=7,
        /// Command
        pub command: u8 [VdmCommand] @ 0..=4,
    }
}

impl VdmHeaderStructured {
    pub fn to_bytes(self, buf: &mut [u8]) {
        LittleEndian::write_u32(buf, self.0);
    }

    /// A structured VDM header for SVID `svid`, version 2.1, with the given command and command type.
    pub fn new(svid: u16, command: VdmCommand, command_type: VdmCommandType) -> Self {
        Self::default()
            .with_standard_or_vid(svid)
            .with_vdm_version_major(VdmVersionMajor::Version2x.into())
            .with_vdm_version_minor(VdmVersionMinor::Version21.into())
            .with_command(command)
            .with_command_type(command_type)
    }

    /// The header of a response to this request: same SVID, version, object position and command.
    pub fn response(self, command_type: VdmCommandType) -> Self {
        self.with_command_type(command_type)
    }
}

impl Default for VdmHeaderStructured {
    fn default() -> Self {
        Self(0).with_vdm_type(VdmType::Structured)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum VdmVersionMajor {
    Version10,
    Version2x,
    /// A reserved value.
    Reserved(u8),
}

impl From<VdmVersionMajor> for u8 {
    fn from(value: VdmVersionMajor) -> Self {
        match value {
            VdmVersionMajor::Version10 => 0b00,
            VdmVersionMajor::Version2x => 0b01,
            VdmVersionMajor::Reserved(raw) => raw & 0b11,
        }
    }
}

impl From<u8> for VdmVersionMajor {
    fn from(value: u8) -> Self {
        match value {
            0b00 => VdmVersionMajor::Version10,
            0b01 => VdmVersionMajor::Version2x,
            other => VdmVersionMajor::Reserved(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum VdmVersionMinor {
    Version20,
    Version21,
    /// A reserved value.
    Reserved(u8),
}

impl From<VdmVersionMinor> for u8 {
    fn from(value: VdmVersionMinor) -> Self {
        match value {
            VdmVersionMinor::Version20 => 0b00,
            VdmVersionMinor::Version21 => 0b01,
            VdmVersionMinor::Reserved(raw) => raw & 0b11,
        }
    }
}

impl From<u8> for VdmVersionMinor {
    fn from(value: u8) -> Self {
        match value {
            0b00 => VdmVersionMinor::Version20,
            0b01 => VdmVersionMinor::Version21,
            other => VdmVersionMinor::Reserved(other),
        }
    }
}

bitfield! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    pub struct VdmHeaderUnstructured(pub u32): FromStorage, IntoStorage {
        /// Vdm Standard or Vendor ID
        pub standard_or_vid: u16 @ 16..=31,
        /// Vdm Type (Unstructured/Structured)
        pub vdm_type: bool [VdmType] @ 15,
        /// Message defined
        pub data: u16 @ 0..=14
    }
}

impl VdmHeaderUnstructured {
    pub fn to_bytes(self, buf: &mut [u8]) {
        LittleEndian::write_u32(buf, self.0);
    }
}

bitfield! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    pub struct VdmIdentityHeader(pub u32): FromStorage, IntoStorage {
        /// Host data capable
        pub host_data: bool @ 31,
        /// Device data capable
        pub device_data: bool @ 30,
        /// Product type UFP
        pub product_type_ufp: u8 [SopProductTypeUfp] @ 27..=29,
        /// Modal Operation Supported
        pub modal_supported: bool @ 26,
        /// Product type DFP
        pub product_type_dfp: u8 [SopProductTypeDfp] @ 23..=25,
        /// Connector type
        pub connector_type: u8 [ConnectorType] @ 21..=22,
        /// VID
        pub vid: u16 @ 0..=15,
    }
}

impl VdmIdentityHeader {
    pub fn to_bytes(self, buf: &mut [u8]) {
        LittleEndian::write_u32(buf, self.0);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum SopProductTypeUfp {
    NotUFP,
    PdUsbHub,
    PdUsbPeripheral,
    Psd,
    /// A reserved value, or a cable plug type when the header came from SOP'/SOP''.
    Reserved(u8),
}

impl From<SopProductTypeUfp> for u8 {
    fn from(value: SopProductTypeUfp) -> Self {
        match value {
            SopProductTypeUfp::NotUFP => 0b000,
            SopProductTypeUfp::PdUsbHub => 0b001,
            SopProductTypeUfp::PdUsbPeripheral => 0b010,
            SopProductTypeUfp::Psd => 0b011,
            SopProductTypeUfp::Reserved(raw) => raw & 0b111,
        }
    }
}

impl From<u8> for SopProductTypeUfp {
    fn from(value: u8) -> Self {
        match value {
            0b000 => SopProductTypeUfp::NotUFP,
            0b001 => SopProductTypeUfp::PdUsbHub,
            0b010 => SopProductTypeUfp::PdUsbPeripheral,
            0b011 => SopProductTypeUfp::Psd,
            other => SopProductTypeUfp::Reserved(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum SopProductTypeDfp {
    NotDFP,
    PDUSBHub,
    PDUSBHost,
    PowerBrick,
    /// A reserved value.
    Reserved(u8),
}

impl From<SopProductTypeDfp> for u8 {
    fn from(value: SopProductTypeDfp) -> Self {
        match value {
            SopProductTypeDfp::NotDFP => 0b000,
            SopProductTypeDfp::PDUSBHub => 0b001,
            SopProductTypeDfp::PDUSBHost => 0b010,
            SopProductTypeDfp::PowerBrick => 0b011,
            SopProductTypeDfp::Reserved(raw) => raw & 0b111,
        }
    }
}

impl From<u8> for SopProductTypeDfp {
    fn from(value: u8) -> Self {
        match value {
            0b000 => SopProductTypeDfp::NotDFP,
            0b001 => SopProductTypeDfp::PDUSBHub,
            0b010 => SopProductTypeDfp::PDUSBHost,
            0b011 => SopProductTypeDfp::PowerBrick,
            other => SopProductTypeDfp::Reserved(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum ConnectorType {
    USBTypeCReceptacle,
    USBTypeCPlug,
    /// A reserved value (`00b` is used by PD 2.0 and 3.0 partners, which do not fill the field).
    Reserved(u8),
}

impl From<ConnectorType> for u8 {
    fn from(value: ConnectorType) -> Self {
        match value {
            ConnectorType::USBTypeCReceptacle => 0b10,
            ConnectorType::USBTypeCPlug => 0b11,
            ConnectorType::Reserved(raw) => raw & 0b11,
        }
    }
}

impl From<u8> for ConnectorType {
    fn from(value: u8) -> Self {
        match value {
            0b10 => ConnectorType::USBTypeCReceptacle,
            0b11 => ConnectorType::USBTypeCPlug,
            other => ConnectorType::Reserved(other),
        }
    }
}

bitfield! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    pub struct CertStatVDO(pub u32): FromStorage, IntoStorage {
        /// XID
        pub xid: u32 @ 0..=31,
    }
}

impl CertStatVDO {
    pub fn to_bytes(self, buf: &mut [u8]) {
        LittleEndian::write_u32(buf, self.0);
    }
}

bitfield! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    pub struct ProductVDO(pub u32): FromStorage, IntoStorage {
        /// USB Product ID
        pub pid: u16 @ 16..=31,
        pub bcd_device: u16 @ 0..=15,
    }
}

impl ProductVDO {
    pub fn to_bytes(self, buf: &mut [u8]) {
        LittleEndian::write_u32(buf, self.0);
    }
}

bitfield! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    pub struct UFPTypeVDO(pub u32): FromStorage, IntoStorage {
        /// USB Product ID
        pub version: u8 @ 29..=31,
        pub device_capability: u8 @ 24..=27,
        pub vconn_power: u8 @ 8..=10,
        pub vconn_required: bool @ 7,
        pub vbus_required: bool @ 6,
        pub alternate_modes: u8 @ 3..=5,
        pub usb_highest_speed: u8 @ 0..=2,
    }
}

impl UFPTypeVDO {
    pub fn to_bytes(self, buf: &mut [u8]) {
        LittleEndian::write_u32(buf, self.0);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum USBHighestSpeed {
    USB20Only,
    USB32Gen1,
    USB32Gen2,
    USB40Gen3,
    USB40Gen4,
    /// A reserved value.
    Reserved(u8),
}

impl From<USBHighestSpeed> for u8 {
    fn from(value: USBHighestSpeed) -> Self {
        match value {
            USBHighestSpeed::USB20Only => 0b000,
            USBHighestSpeed::USB32Gen1 => 0b001,
            USBHighestSpeed::USB32Gen2 => 0b010,
            USBHighestSpeed::USB40Gen3 => 0b011,
            USBHighestSpeed::USB40Gen4 => 0b100,
            USBHighestSpeed::Reserved(raw) => raw & 0b111,
        }
    }
}

impl From<u8> for USBHighestSpeed {
    fn from(value: u8) -> Self {
        match value {
            0b000 => USBHighestSpeed::USB20Only,
            0b001 => USBHighestSpeed::USB32Gen1,
            0b010 => USBHighestSpeed::USB32Gen2,
            0b011 => USBHighestSpeed::USB40Gen3,
            0b100 => USBHighestSpeed::USB40Gen4,
            other => USBHighestSpeed::Reserved(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum VconnPower {
    P1W,
    P1_5W,
    P2W,
    P3W,
    P4W,
    P5W,
    P6W,
    /// A reserved value.
    Reserved(u8),
}

impl From<VconnPower> for u8 {
    fn from(value: VconnPower) -> Self {
        match value {
            VconnPower::P1W => 0b000,
            VconnPower::P1_5W => 0b001,
            VconnPower::P2W => 0b010,
            VconnPower::P3W => 0b011,
            VconnPower::P4W => 0b100,
            VconnPower::P5W => 0b101,
            VconnPower::P6W => 0b110,
            VconnPower::Reserved(raw) => raw & 0b111,
        }
    }
}

impl From<u8> for VconnPower {
    fn from(value: u8) -> Self {
        match value {
            0b000 => VconnPower::P1W,
            0b001 => VconnPower::P1_5W,
            0b010 => VconnPower::P2W,
            0b011 => VconnPower::P3W,
            0b100 => VconnPower::P4W,
            0b101 => VconnPower::P5W,
            0b110 => VconnPower::P6W,
            other => VconnPower::Reserved(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum UFPVDOVersion {
    Version1_3,
    /// Any other version value.
    Other(u8),
}

impl From<UFPVDOVersion> for u8 {
    fn from(value: UFPVDOVersion) -> Self {
        match value {
            UFPVDOVersion::Version1_3 => 0b011,
            UFPVDOVersion::Other(raw) => raw & 0b111,
        }
    }
}

impl From<u8> for UFPVDOVersion {
    fn from(value: u8) -> Self {
        match value {
            0b011 => UFPVDOVersion::Version1_3,
            other => UFPVDOVersion::Other(other),
        }
    }
}

bitfield! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    pub struct DisplayPortCapabilities(pub u32): FromStorage, IntoStorage {
        /// UFP_D Pin Assignments Supported
        pub ufp_d_pin_assignments: u8 @ 16..=23,
        /// DFP_D Pin Assignments Supported
        pub dfp_d_pin_assignments: u8 @ 8..=15,
        /// USB r2.0 Signalling Not Used
        pub usb20_signalling_not_used: bool @ 7,
        /// Receptacle Indication
        pub receptacle_indication: bool @ 6,
        /// Signalling for Transport of DisplayPort Protocol
        pub signaling_rate: u8 @ 2..=5,
        /// Port Capability
        pub capability: u8 @ 0..=1,
    }
}

impl DisplayPortCapabilities {
    pub fn to_bytes(self, buf: &mut [u8]) {
        LittleEndian::write_u32(buf, self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every raw value of every decoded field converts without panicking, and known values round-trip.
    #[test]
    fn all_raw_values_decode() {
        for raw in 0..=u8::MAX {
            let _ = VdmCommandType::from(raw);
            let _ = SopProductTypeUfp::from(raw & 0b111);
            let _ = SopProductTypeDfp::from(raw & 0b111);
            let _ = ConnectorType::from(raw & 0b11);
            let _ = USBHighestSpeed::from(raw & 0b111);
            let _ = VconnPower::from(raw & 0b111);
            let _ = UFPVDOVersion::from(raw & 0b111);
            let _ = VdmVersionMajor::from(raw & 0b11);
            let _ = VdmVersionMinor::from(raw & 0b11);
            let command = VdmCommand::from(raw & 0x1F);
            assert_eq!(u8::from(command), raw & 0x1F);
        }
    }

    /// Bit patterns that used to panic in the getters (reserved commands, connector type 00b) now decode.
    #[test]
    fn reserved_getters_do_not_panic() {
        let header = VdmHeaderStructured(0xFF00_8000 | 0x1F);
        assert_eq!(header.command(), VdmCommand::Other(0x1F));

        let id = VdmIdentityHeader(0xFFFF_FFFF);
        assert_eq!(id.product_type_ufp(), SopProductTypeUfp::Reserved(0b111));
        assert_eq!(id.product_type_dfp(), SopProductTypeDfp::Reserved(0b111));
        assert_eq!(VdmIdentityHeader(0).connector_type(), ConnectorType::Reserved(0));
    }

    #[test]
    fn vendor_defined_round_trip() {
        let header = VdmHeaderStructured::new(PD_SID, VdmCommand::DiscoverIdentity, VdmCommandType::ResponderACK);
        let vdm = VendorDefined::new(VdmHeader::Structured(header), &[1, 2, 3]);
        assert_eq!(vdm.num_objects(), 4);

        let mut buf = [0u8; 28];
        let len = vdm.to_bytes(&mut buf);
        assert_eq!(len, 16);

        let parsed = VendorDefined::parse(&buf[..len], 4).unwrap();
        assert_eq!(parsed, vdm);
        let structured = parsed.structured().unwrap();
        assert_eq!(structured.command(), VdmCommand::DiscoverIdentity);
        assert_eq!(structured.command_type(), VdmCommandType::ResponderACK);
        assert_eq!(structured.standard_or_vid(), PD_SID);
    }

    #[test]
    fn vendor_defined_parse_limits() {
        assert!(VendorDefined::parse(&[0, 0], 1).is_none());
        // More objects claimed than present: only what is there is read.
        let buf = [0u8; 12];
        assert_eq!(VendorDefined::parse(&buf, 7).unwrap().vdos.len(), 2);
        // Never more than six VDOs.
        let buf = [0u8; 40];
        assert_eq!(VendorDefined::parse(&buf, 7).unwrap().vdos.len(), MAX_VDOS);
    }
}
