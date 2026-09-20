use bitcoin::secp256k1::ecdsa::Signature;

use super::tlv::{check_extensions, read_tlvs, write_tlvs, Reader, Writer};
use super::{lifecycle, setup, Header, Message, Payload, WireError, MAX_MESSAGE_LEN};

pub(super) fn decode(wire: &[u8]) -> Result<Message, WireError> {
	if wire.len() > MAX_MESSAGE_LEN {
		return Err(WireError::SizeLimit);
	}
	if wire.len() < 2 + 64 + 64 {
		return Err(WireError::Truncated);
	}
	let signature_start = wire.len() - 64;
	let signature = wire[signature_start..].try_into().map_err(|_| WireError::Truncated)?;
	validate_signature_encoding(&signature)?;
	let mut reader = Reader::new(&wire[..signature_start]);
	let kind = reader.u16()?;
	let header = Header { channel_id: reader.array()?, epoch_id: reader.array()? };
	let (payload, extensions) = match kind {
		55001 => setup::read_init(&mut reader)?,
		55003 => setup::read_accept(&mut reader)?,
		55045 | 55047 | 55049 | 55051 | 55053 => lifecycle::read(kind, &mut reader)?,
		_ => return Err(WireError::UnsupportedMessage(kind)),
	};
	check_extensions(&extensions)?;
	Ok(Message { header, payload, extensions, signature })
}

pub(super) fn encode(message: &Message) -> Result<Vec<u8>, WireError> {
	validate_signature_encoding(&message.signature)?;
	let mut writer = Writer(encode_unsigned(message)?);
	writer.put(&message.signature)?;
	Ok(writer.0)
}

pub(super) fn encode_unsigned(message: &Message) -> Result<Vec<u8>, WireError> {
	check_extensions(&message.extensions)?;
	let mut writer = Writer::new();
	writer.u16(message.message_type())?;
	writer.put(&message.header.channel_id)?;
	writer.put(&message.header.epoch_id)?;
	let known = match &message.payload {
		Payload::Init(init) => setup::write_init(init, &mut writer)?,
		Payload::Accept(accept) => setup::write_accept(accept, &mut writer)?,
		payload => lifecycle::write(payload, &mut writer)?,
	};
	if matches!(message.payload, Payload::Init(_)) {
		for tlv in &message.extensions {
			if matches!(tlv.kind, 1 | 3 | 5 | 9 | 13 | 15) {
				return Err(WireError::InvalidTlv(tlv.kind));
			}
		}
	}
	if matches!(message.payload, Payload::CloseAck(_))
		&& message.extensions.iter().any(|tlv| tlv.kind == 1)
	{
		return Err(WireError::InvalidTlv(1));
	}
	write_tlvs(&mut writer, known, &message.extensions)?;
	if writer.0.len() > MAX_MESSAGE_LEN - 64 {
		return Err(WireError::SizeLimit);
	}
	Ok(writer.0)
}

fn validate_signature_encoding(bytes: &[u8; 64]) -> Result<(), WireError> {
	if bytes[..32].iter().all(|byte| *byte == 0) || bytes[32..].iter().all(|byte| *byte == 0) {
		return Err(WireError::InvalidSignature);
	}
	let signature = Signature::from_compact(bytes).map_err(|_| WireError::InvalidSignature)?;
	let mut normalized = signature;
	normalized.normalize_s();
	if normalized != signature {
		return Err(WireError::InvalidSignature);
	}
	Ok(())
}

pub(super) fn remaining_tlvs(reader: &mut Reader<'_>) -> Result<Vec<super::Tlv>, WireError> {
	read_tlvs(reader)
}
