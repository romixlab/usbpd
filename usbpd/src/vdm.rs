//! Vendor defined messages (VDM) between the policy engines and the device policy manager.
//!
//! The policy engines do not run a VDM state machine of their own. In the Ready state:
//! - every received `Vendor_Defined` message, on any SOP, goes to
//!   [`VdmDevicePolicyManager::vdm_received`], which answers it or not;
//! - the DPM starts a VDM with `Event::SendVdm` (sink and source), and gets the response through `vdm_received`
//!   like any other VDM.
//!
//! Discovery (Discover Identity, SVIDs and Modes), mode entry and exit, Attention and the
//! VDMResponseTimer (tVDMSenderResponse) are the DPM's to run. The default implementation answers
//! `Not_Supported`, which is what the engines did before VDMs were forwarded.
use core::future::Future;

pub use usbpd_traits::Sop;

pub use crate::protocol_layer::message::data::vendor_defined::{
    DISPLAYPORT_SID, MAX_VDOS, PD_SID, VdmCommand, VdmCommandType, VdmHeader, VdmHeaderStructured,
    VdmHeaderUnstructured, VendorDefined,
};

/// How the policy engine answers a received VDM.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum VdmResponse {
    /// Send this VDM on the SOP the received one came on, e.g. the ACK, NAK or BUSY to a request.
    ///
    /// Per PD 3.2 Sec. 6.6.6.1, a responder answers within tVDMReceiverResponse (15 ms).
    Reply(VendorDefined),
    /// Send nothing: the VDM was a response to our own request, an Attention, or is ignored on purpose.
    Ignore,
    /// Answer `Not_Supported` (per PD 3.2 Sec. 6.4.4.1). On cable SOPs the VDM is ignored instead, since only a
    /// cable plug answers there.
    NotSupported,
}

/// The device policy manager's side of VDMs, shared by the sink and the source policy engines.
///
/// Leave the implementation empty to answer every VDM with `Not_Supported`.
pub trait VdmDevicePolicyManager {
    /// A `Vendor_Defined` message was received on `sop`.
    fn vdm_received(&mut self, _sop: Sop, _vdm: &VendorDefined) -> impl Future<Output = VdmResponse> {
        async { VdmResponse::NotSupported }
    }

    /// A VDM from `Event::SendVdm` or a [`VdmResponse::Reply`] could not be sent on a cable SOP (no GoodCRC: no
    /// cable plug answers there). On SOP, a failed transmission makes the engine soft reset as usual.
    fn vdm_not_sent(&mut self, _sop: Sop) -> impl Future<Output = ()> {
        async {}
    }
}
