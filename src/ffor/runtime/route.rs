//! Public witness-to-settlement route evidence from the node's own network graph.
//!
//! Only a fully signed `ChannelAnnouncement` and an enabled, signed witness-sourced
//! `ChannelUpdate` are acceptable. Rapid gossip sync retains neither, so the runtime refuses to
//! issue an invoice rather than fabricate advisory route terms. Native revalidates everything.

use bitcoin::secp256k1::PublicKey;
use lightning::ln::ffor::FFORWitnessRouteEvidence;
use lightning::routing::gossip::NodeId;

use crate::types::Graph;

/// Find signed gossip for a public channel between `witness` and `settlement` whose update was
/// signed by the witness (source == witness). Returns `None` when no such evidence is retained.
pub(super) fn witness_route_evidence(
	graph: &Graph, witness: PublicKey, settlement: PublicKey,
) -> Option<FFORWitnessRouteEvidence> {
	let witness_id = NodeId::from_pubkey(&witness);
	let settlement_id = NodeId::from_pubkey(&settlement);
	let read = graph.read_only();
	let node = read.node(&witness_id)?;
	for scid in node.channels.iter() {
		let Some(info) = read.channel(*scid) else { continue };
		let direction = if info.node_one == witness_id && info.node_two == settlement_id {
			info.one_to_two.as_ref()
		} else if info.node_one == settlement_id && info.node_two == witness_id {
			info.two_to_one.as_ref()
		} else {
			continue;
		};
		let (Some(announcement), Some(direction)) = (info.announcement_message.as_ref(), direction)
		else {
			continue;
		};
		if !direction.enabled {
			continue;
		}
		let Some(update) = direction.last_update_message.as_ref() else { continue };
		return Some(FFORWitnessRouteEvidence {
			announcement: announcement.clone(),
			update: update.clone(),
		});
	}
	None
}
