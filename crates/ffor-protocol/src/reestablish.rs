//! The Variant D value carried by `channel_reestablish` TLV 55001 (FFOR section 11.1).
//!
//! This is an unsigned peer report, not evidence of activation or payment. The channel engine
//! must compare it with its durable epoch and authenticate any replayed acknowledgement. In
//! particular, Variant D's zero sequence number never permits discarding an active epoch.

use std::fmt;

use crate::transcript::Digest;

/// The TLV type within a standard BOLT 2 `channel_reestablish` message.
pub const TLV_TYPE: u64 = 55001;
/// The exact value length, excluding the enclosing TLV type and length.
pub const VALUE_LEN: usize = 67;

/// A reported lifecycle phase, without authority to perform a channel transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ReportedState {
	/// Setup has started but its vouchers are not yet committed.
	Negotiating = 0,
	/// The complete book exists in both commitment views.
	VouchersCommitted = 1,
	/// The receiver is waiting for the signed activation acknowledgement.
	Activating = 2,
	/// The reporting peer says it durably accepted activation.
	Active = 3,
	/// The reporting peer says it accepted the close acknowledgement.
	Draining = 4,
	/// All vouchers have been irrevocably resolved.
	Closed = 5,
	/// Setup was aborted before activation.
	Aborted = 6,
}

/// The reported epoch and phase on reconnect.
///
/// No signature is included here. Parsing preserves the reported hash even in a pre-active
/// state, where it has no activation authority. This also permits reading the pinned Beignet
/// implementation, which retains its computed hash while reporting `Activating`.
///
/// Locally generated reports must use a zero hash before `Active`, as required by section 11.1.
/// `last_seq` is not exposed because Variant D always encodes zero. Ordinary BOLT 2 commitment
/// and revocation counters still need their normal validation by the channel engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reestablish {
	/// The epoch being reported, scoped by the enclosing message's channel ID.
	pub epoch_id: Digest,
	/// The peer's reported lifecycle state.
	pub state: ReportedState,
	/// Reported activation digest, meaningful only with appropriate authenticated evidence.
	pub activation_hash: Digest,
}

impl Reestablish {
	/// Decode one exact TLV value. Reject truncation, trailing bytes, unknown states and A/B sequences.
	///
	/// This function allocates nothing and does not authorize any channel transition.
	pub fn decode(value: &[u8]) -> Result<Self, ReestablishError> {
		let value: &[u8; VALUE_LEN] = value.try_into().map_err(|_| ReestablishError::Length)?;
		let state = match value[32] {
			0 => ReportedState::Negotiating,
			1 => ReportedState::VouchersCommitted,
			2 => ReportedState::Activating,
			3 => ReportedState::Active,
			4 => ReportedState::Draining,
			5 => ReportedState::Closed,
			6 => ReportedState::Aborted,
			_ => return Err(ReestablishError::State),
		};
		if value[33..35] != [0, 0] {
			return Err(ReestablishError::Sequence);
		}
		let mut epoch_id = [0; 32];
		epoch_id.copy_from_slice(&value[..32]);
		let mut activation_hash = [0; 32];
		activation_hash.copy_from_slice(&value[35..]);
		Ok(Self { epoch_id, state, activation_hash })
	}

	/// Encode this report exactly, with Variant D's zero sequence number.
	///
	/// Use only engine-owned durable state to construct an outbound report. For an active receiver,
	/// a missing or conflicting peer report is not permission to discard vouchers or lift the freeze.
	///
	/// ```
	/// use ffor_protocol::reestablish::{Reestablish, ReportedState};
	/// let report = Reestablish {
	///     epoch_id: [1; 32], state: ReportedState::Activating, activation_hash: [0; 32],
	/// };
	/// assert_eq!(Reestablish::decode(&report.encode()).unwrap(), report);
	/// ```
	pub fn encode(&self) -> [u8; VALUE_LEN] {
		let mut value = [0; VALUE_LEN];
		value[..32].copy_from_slice(&self.epoch_id);
		value[32] = self.state as u8;
		value[35..].copy_from_slice(&self.activation_hash);
		value
	}
}

/// Why a reported reconnect value is not a supported Variant D encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReestablishError {
	/// The input was not exactly 67 bytes.
	Length,
	/// The state byte did not identify one of the seven specified states.
	State,
	/// Variant D requires `last_seq` to equal zero.
	Sequence,
}

impl fmt::Display for ReestablishError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::Length => "FFOR reconnect value must be exactly 67 bytes",
			Self::State => "unknown FFOR reconnect state",
			Self::Sequence => "Variant D reconnect sequence must be zero",
		})
	}
}

impl std::error::Error for ReestablishError {}
