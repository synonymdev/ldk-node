//! Real native W generations and persistence barriers, with crash cuts between the two stores.

use super::*;

fn retain_sidecar_only(h: &mut Harness, request: &Provision) -> Vec<u8> {
	let w = h.policies[0].witness;
	let ack = Acknowledgement::new(
		request.request_id(),
		AcknowledgementResult::Accepted {
			witness: w,
			retention_until: request.manifest().unsigned().parameters().retention_until + 1,
		},
	)
	.unwrap();
	let message = h.receive(w, &ack);
	let (_, checked) = h.owner.pending.check(&message, &ack).unwrap();
	let binding = WitnessStorageBinding::from_native_context(&h.context).unwrap();
	h.owner.store.acknowledge(&binding, &checked).unwrap();
	assert!(h
		.node
		.channel_manager
		.ffor_receiver_witness_acknowledgements(&h.context)
		.unwrap()
		.unwrap()
		.acknowledgements()
		.is_empty());
	h.secret_bytes()
}

#[test]
fn ffor_witness_owner_first_ack_ids_join_after_crash_or_disconnect() {
	for restart in [false, true] {
		let mut h = Harness::new();
		h.register_and_persist();
		let w = h.policies[0].witness;
		h.connect(w);
		assert_eq!(h.owner.provision(&h.context, w), Ok(ProvisioningProgress::Queued));
		let first = h.outgoing().remove(0);
		let first_envelope = retain_sidecar_only(&mut h, &first);
		if restart {
			// The durable manager still has registration but no native ACK; the protected write
			// above succeeded. A new owner must confirm it and receive a genuinely fresh W token.
			let storage = Arc::clone(&h.storage);
			drop(h);
			h = Harness::from_store(storage);
			h.connect(w);
			assert_eq!(
				h.owner.provision(&h.context, w),
				Ok(ProvisioningProgress::AwaitingPersistence)
			);
			h.persist_manager();
		} else {
			h.disconnect(w);
			h.connect(w);
		}
		assert_eq!(h.owner.provision(&h.context, w), Ok(ProvisioningProgress::Queued));
		let second = h.outgoing().remove(0);
		assert_ne!(first.request_id(), second.request_id());
		assert_eq!(first.manifest(), second.manifest());
		let ack = Acknowledgement::new(
			second.request_id(),
			AcknowledgementResult::Accepted {
				witness: w,
				retention_until: second.manifest().unsigned().parameters().retention_until + 2,
			},
		)
		.unwrap();
		let response = h.receive(w, &ack);
		assert_eq!(
			h.owner.acknowledge(&response),
			Ok(AcknowledgementProgress::AwaitingPersistence)
		);
		assert_eq!(h.secret_bytes(), first_envelope, "first sidecar promise must remain immutable");
		let native = h
			.node
			.channel_manager
			.ffor_receiver_witness_acknowledgements(&h.context)
			.unwrap()
			.unwrap();
		assert_eq!(native.acknowledgements()[0].request_id(), second.request_id());
		assert_eq!(h.owner.provision(&h.context, w), Ok(ProvisioningProgress::AwaitingPersistence));
		h.persist_manager();
		assert_eq!(
			h.owner.provision(&h.context, w),
			Ok(ProvisioningProgress::AcknowledgementRetained)
		);
		assert_eq!(h.owner.pending.usage(), (0, 0));
		assert!(h.outgoing().is_empty());
		// A fresh restored barrier still applies even when both historical promises exist.
		let storage = Arc::clone(&h.storage);
		drop(h);
		let mut restored = Harness::from_store(storage);
		assert_eq!(
			restored.owner.provision(&restored.context, w),
			Ok(ProvisioningProgress::AwaitingPersistence)
		);
		restored.persist_manager();
		assert_eq!(
			restored.owner.provision(&restored.context, w),
			Ok(ProvisioningProgress::AcknowledgementRetained)
		);
	}
}

#[test]
fn ffor_witness_owner_native_capacity_refusal_preserves_predecessor() {
	let mut h = Harness::new();
	h.register_and_persist();
	let w = h.policies[0].witness;
	h.connect(w);
	let mut previous = None;
	for _ in 0..64 {
		assert_eq!(h.owner.retry_provision(&h.context, w), Ok(ProvisioningProgress::Queued));
		previous = Some(h.outgoing().remove(0));
	}
	let previous = previous.unwrap();
	let usage = h.owner.pending.usage();
	assert!(matches!(h.owner.retry_provision(&h.context, w), Err(WitnessOwnerError::Native(_))));
	assert_eq!(h.owner.pending.usage(), usage);
	assert!(h.owner.pending.contains_request_id(previous.request_id()));
	assert!(h.outgoing().is_empty());
	let response = h.receive(w, &h.accepted(&previous));
	assert_eq!(h.owner.acknowledge(&response), Ok(AcknowledgementProgress::AwaitingPersistence));
	h.persist_manager();
	assert_eq!(h.owner.acknowledge(&response), Ok(AcknowledgementProgress::Retained));
}

#[test]
fn ffor_witness_owner_correlated_refusal_retires_exact_attempt() {
	let mut h = Harness::new();
	h.register_and_persist();
	let w = h.policies[0].witness;
	h.connect(w);
	assert_eq!(h.owner.provision(&h.context, w), Ok(ProvisioningProgress::Queued));
	let request = h.outgoing().remove(0);
	let refused =
		Acknowledgement::new(request.request_id(), AcknowledgementResult::Refused(Vec::new()))
			.unwrap();
	let response = h.receive(w, &refused);
	assert_eq!(h.owner.acknowledge(&response), Ok(AcknowledgementProgress::Refused));
	assert_eq!(h.owner.pending.usage(), (0, 0));
	let connection = h.owner.transport.native_connection(w).unwrap();
	assert!(h
		.node
		.channel_manager
		.retain_ffor_receiver_witness_ack(connection.native(), &h.accepted(&request))
		.is_err());
	assert_eq!(h.owner.retry_provision(&h.context, w), Ok(ProvisioningProgress::Queued));
	assert_ne!(h.outgoing().remove(0).request_id(), request.request_id());
}

#[test]
fn ffor_witness_owner_native_disconnect_after_storage_keeps_first_promise() {
	let mut h = Harness::new();
	h.register_and_persist();
	let w = h.policies[0].witness;
	h.connect(w);
	assert_eq!(h.owner.provision(&h.context, w), Ok(ProvisioningProgress::Queued));
	let request = h.outgoing().remove(0);
	let response = h.receive(w, &h.accepted(&request));
	let before = h.secret_bytes();
	// Native disconnect precedes the custom handler callback in PeerManager. The old Node
	// token is briefly present, but native must reject its captured generation after storage.
	h.node.channel_manager.peer_disconnected(w);
	assert!(matches!(h.owner.acknowledge(&response), Err(WitnessOwnerError::Native(_))));
	assert_ne!(before, h.secret_bytes());
	assert!(h
		.node
		.channel_manager
		.ffor_receiver_witness_acknowledgements(&h.context)
		.unwrap()
		.unwrap()
		.acknowledgements()
		.is_empty());
	h.handler.peer_disconnected(w);
	h.connect(w);
	assert_eq!(h.owner.provision(&h.context, w), Ok(ProvisioningProgress::Queued));
	assert_ne!(h.outgoing().remove(0).request_id(), request.request_id());
}

#[test]
fn ffor_witness_owner_join_requires_barrier_after_historical_storage_reads() {
	let mut h = Harness::new();
	h.register_and_persist();
	let w = h.policies[0].witness;
	h.connect(w);
	assert_eq!(h.owner.provision(&h.context, w), Ok(ProvisioningProgress::Queued));
	let request = h.outgoing().remove(0);
	retain_sidecar_only(&mut h, &request);
	let manager = Arc::clone(&h.node.channel_manager);
	let connection = h.owner.transport.native_connection(w).unwrap();
	let ack = h.accepted(&request);
	let mut secret_reads = 0;
	let observed = Arc::new(std::sync::Mutex::new(None));
	let requirement = Arc::clone(&observed);
	*h.storage.read_hook.lock().unwrap() = Some(Box::new(move |namespace| {
		if namespace == "ffor_witness" {
			secret_reads += 1;
			if secret_reads == 2 {
				// Retain through the actual native API during the second protected read, after the
				// initial registration barrier was observed but before the historical ACK join.
				*requirement.lock().unwrap() = Some(
					manager.retain_ffor_receiver_witness_ack(connection.native(), &ack).unwrap(),
				);
			}
		}
	}));
	assert!(matches!(h.owner.provision(&h.context, w), Err(WitnessOwnerError::Native(_))));
	*h.storage.read_hook.lock().unwrap() = None;
	let requirement = observed.lock().unwrap().take().expect("native ACK was retained during join");
	assert!(!h.node.channel_manager.is_ffor_state_persisted(&requirement));
	assert!(h.outgoing().is_empty());
	h.persist_manager();
	assert_eq!(h.owner.provision(&h.context, w), Ok(ProvisioningProgress::AcknowledgementRetained));
}
