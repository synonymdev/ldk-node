use super::codec::remaining_tlvs;
use super::setup::check_count;
use super::tlv::{optional, Reader, Writer};
use super::{Abort, Activate, CloseAck, Payload, Preimage, Tlv, WireError, MAX_MESSAGE_LEN};

pub(super) fn read(kind: u16, reader: &mut Reader<'_>) -> Result<(Payload, Vec<Tlv>), WireError> {
	let payload = match kind {
		55045 => Payload::Activate(Activate {
			setup_hash: reader.array()?,
			book_hash: reader.array()?,
			commit_hash: reader.array()?,
			epoch_start_height: reader.u32()?,
		}),
		55047 => Payload::ActivateAck(reader.array()?),
		55049 => {
			let transcript_hash = reader.array()?;
			let reason = reader.u16()?;
			if reason > 7 {
				return Err(WireError::InvalidField);
			}
			let data_len = usize::from(reader.u16()?);
			Payload::Abort(Abort { transcript_hash, reason, data: reader.take(data_len)?.to_vec() })
		},
		55051 => Payload::Close(reader.array()?),
		55053 => return read_close_ack(reader),
		_ => return Err(WireError::UnsupportedMessage(kind)),
	};
	Ok((payload, remaining_tlvs(reader)?))
}

fn read_close_ack(reader: &mut Reader<'_>) -> Result<(Payload, Vec<Tlv>), WireError> {
	let activation_hash = reader.array()?;
	let num_slots = reader.u16()?;
	check_count(usize::from(num_slots))?;
	let settled = reader.take(usize::from(num_slots).div_ceil(8))?.to_vec();
	let mut tlvs = remaining_tlvs(reader)?;
	let value = optional(&mut tlvs, 1);
	let preimages_tlv_present = value.is_some();
	let bytes = value.unwrap_or_default();
	if bytes.len() % 34 != 0 || bytes.len() / 34 > usize::from(num_slots) {
		return Err(WireError::InvalidTlv(1));
	}
	let mut preimages = Vec::new();
	let mut records = Reader::new(&bytes);
	while !records.is_empty() {
		preimages.push(Preimage { slot: records.u16()?, value: records.array()? });
	}
	let ack = CloseAck { activation_hash, num_slots, settled, preimages, preimages_tlv_present };
	validate_close_ack(&ack)?;
	Ok((Payload::CloseAck(ack), tlvs))
}

pub(super) fn write(payload: &Payload, writer: &mut Writer) -> Result<Vec<Tlv>, WireError> {
	match payload {
		Payload::Activate(activate) => {
			writer.put(&activate.setup_hash)?;
			writer.put(&activate.book_hash)?;
			writer.put(&activate.commit_hash)?;
			writer.u32(activate.epoch_start_height)?;
		},
		Payload::ActivateAck(hash) | Payload::Close(hash) => writer.put(hash)?,
		Payload::Abort(abort) => {
			if abort.reason > 7 {
				return Err(WireError::InvalidField);
			}
			if abort.data.len() > MAX_MESSAGE_LEN {
				return Err(WireError::SizeLimit);
			}
			writer.put(&abort.transcript_hash)?;
			writer.u16(abort.reason)?;
			writer.u16(abort.data.len() as u16)?;
			writer.put(&abort.data)?;
		},
		Payload::CloseAck(ack) => return write_close_ack(ack, writer),
		_ => return Err(WireError::InvalidField),
	}
	Ok(Vec::new())
}

fn write_close_ack(ack: &CloseAck, writer: &mut Writer) -> Result<Vec<Tlv>, WireError> {
	validate_close_ack(ack)?;
	writer.put(&ack.activation_hash)?;
	writer.u16(ack.num_slots)?;
	writer.put(&ack.settled)?;
	let mut preimages = Writer::new();
	for record in &ack.preimages {
		preimages.u16(record.slot)?;
		preimages.put(&record.value)?;
	}
	Ok(if ack.preimages_tlv_present {
		vec![Tlv { kind: 1, value: preimages.0 }]
	} else {
		Vec::new()
	})
}

fn validate_close_ack(ack: &CloseAck) -> Result<(), WireError> {
	let count = usize::from(ack.num_slots);
	check_count(count)?;
	if ack.settled.len() != count.div_ceil(8) {
		return Err(WireError::InvalidField);
	}
	// Bits outside the book cannot describe slots and must not create alternate encodings.
	let used_bits = count % 8;
	if used_bits != 0 && ack.settled[count / 8] >> used_bits != 0 {
		return Err(WireError::InvalidField);
	}
	let expected = (1..=ack.num_slots).filter(|slot| {
		let bit = usize::from(slot - 1);
		ack.settled[bit / 8] & (1 << (bit % 8)) != 0
	});
	if !expected.eq(ack.preimages.iter().map(|record| record.slot)) {
		return Err(WireError::InvalidTlv(1));
	}
	if !ack.preimages_tlv_present && !ack.preimages.is_empty() {
		return Err(WireError::MissingTlv(1));
	}
	Ok(())
}
