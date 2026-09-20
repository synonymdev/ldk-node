use bitcoin::secp256k1::PublicKey;

use super::codec::remaining_tlvs;
use super::tlv::{fixed, optional, required, Reader, Writer};
use super::{Accept, Init, Payload, Tlv, WireError, MAX_MESSAGE_LEN};
use crate::book::MAX_VOUCHERS;

pub(super) fn read_init(reader: &mut Reader<'_>) -> Result<(Payload, Vec<Tlv>), WireError> {
	let variant = reader.u8()?;
	if variant != 4 {
		return Err(WireError::UnsupportedVariant(variant));
	}
	let budget_msat = reader.u64()?;
	let count = usize::from(reader.u16()?);
	check_count(count)?;
	let min_payment_msat = reader.u64()?;
	let settlement_deadline = reader.u32()?;
	let voucher_expiry = reader.u32()?;
	let fee_base_msat = reader.u32()?;
	let fee_proportional_millionths = reader.u32()?;
	if reader.u64()? != 0 || reader.u16()? != 0 {
		return Err(WireError::InvalidField);
	}
	let mut tlvs = remaining_tlvs(reader)?;
	if let Some(forbidden) = tlvs.iter().find(|entry| matches!(entry.kind, 1 | 3 | 5)) {
		return Err(WireError::InvalidTlv(forbidden.kind));
	}
	let amounts_msat = read_amounts(required(&mut tlvs, 9)?)?;
	if amounts_msat.len() != count {
		return Err(WireError::InvalidTlv(9));
	}
	let witness_peers = optional(&mut tlvs, 13).map(read_points).transpose()?;
	let hash_chain = match optional(&mut tlvs, 15) {
		None => false,
		Some(value) if value == [1] => true,
		Some(_) => return Err(WireError::InvalidTlv(15)),
	};
	let init = Init {
		budget_msat,
		min_payment_msat,
		settlement_deadline,
		voucher_expiry,
		fee_base_msat,
		fee_proportional_millionths,
		amounts_msat,
		witness_peers,
		hash_chain,
	};
	validate_init(&init)?;
	Ok((Payload::Init(init), tlvs))
}

pub(super) fn read_accept(reader: &mut Reader<'_>) -> Result<(Payload, Vec<Tlv>), WireError> {
	let s_commitment_number = reader.u64()?;
	let mut tlvs = remaining_tlvs(reader)?;
	let hashes = required(&mut tlvs, 1)?;
	if hashes.len() % 32 != 0 {
		return Err(WireError::InvalidTlv(1));
	}
	check_count(hashes.len() / 32)?;
	let payment_hashes = hashes
		.chunks_exact(32)
		.map(|hash| hash.try_into().map_err(|_| WireError::InvalidTlv(1)))
		.collect::<Result<_, _>>()?;
	let s_htlc_id_base = u64::from_be_bytes(fixed(required(&mut tlvs, 7)?, 7)?);
	let amounts_msat = read_amounts(required(&mut tlvs, 9)?)?;
	let init_hash = fixed(required(&mut tlvs, 11)?, 11)?;
	let accept =
		Accept { s_commitment_number, payment_hashes, s_htlc_id_base, amounts_msat, init_hash };
	validate_accept(&accept)?;
	Ok((Payload::Accept(accept), tlvs))
}

pub(super) fn write_init(init: &Init, writer: &mut Writer) -> Result<Vec<Tlv>, WireError> {
	validate_init(init)?;
	writer.put(&[4])?;
	writer.u64(init.budget_msat)?;
	writer.u16(init.amounts_msat.len() as u16)?;
	writer.u64(init.min_payment_msat)?;
	writer.u32(init.settlement_deadline)?;
	writer.u32(init.voucher_expiry)?;
	writer.u32(init.fee_base_msat)?;
	writer.u32(init.fee_proportional_millionths)?;
	writer.u64(0)?;
	writer.u16(0)?;
	let mut tlvs = vec![Tlv { kind: 9, value: amount_bytes(&init.amounts_msat) }];
	if let Some(peers) = &init.witness_peers {
		if peers.len() > MAX_MESSAGE_LEN / 33 {
			return Err(WireError::SizeLimit);
		}
		let mut points = Writer::new();
		points.u16(peers.len() as u16)?;
		for peer in peers {
			points.put(&peer.serialize())?;
		}
		tlvs.push(Tlv { kind: 13, value: points.0 });
	}
	if init.hash_chain {
		tlvs.push(Tlv { kind: 15, value: vec![1] });
	}
	Ok(tlvs)
}

pub(super) fn write_accept(accept: &Accept, writer: &mut Writer) -> Result<Vec<Tlv>, WireError> {
	validate_accept(accept)?;
	writer.u64(accept.s_commitment_number)?;
	Ok(vec![
		Tlv { kind: 1, value: accept.payment_hashes.concat() },
		Tlv { kind: 7, value: accept.s_htlc_id_base.to_be_bytes().to_vec() },
		Tlv { kind: 9, value: amount_bytes(&accept.amounts_msat) },
		Tlv { kind: 11, value: accept.init_hash.to_vec() },
	])
}

fn validate_init(init: &Init) -> Result<(), WireError> {
	check_count(init.amounts_msat.len())?;
	if init.voucher_expiry <= init.settlement_deadline {
		return Err(WireError::InvalidField);
	}
	let mut total = 0u64;
	for &amount in &init.amounts_msat {
		if amount == 0 || amount < init.min_payment_msat {
			return Err(WireError::InvalidTlv(9));
		}
		if init.hash_chain && amount != init.amounts_msat[0] {
			return Err(WireError::InvalidTlv(15));
		}
		total = total.checked_add(amount).ok_or(WireError::InvalidField)?;
	}
	if total != init.budget_msat {
		return Err(WireError::InvalidField);
	}
	Ok(())
}

fn validate_accept(accept: &Accept) -> Result<(), WireError> {
	check_count(accept.payment_hashes.len())?;
	if accept.amounts_msat.len() != accept.payment_hashes.len() || accept.amounts_msat.contains(&0)
	{
		return Err(WireError::InvalidTlv(9));
	}
	accept
		.s_htlc_id_base
		.checked_add(accept.payment_hashes.len() as u64 - 1)
		.ok_or(WireError::InvalidTlv(7))?;
	Ok(())
}

pub(super) fn check_count(count: usize) -> Result<(), WireError> {
	if count == 0 || count > MAX_VOUCHERS {
		return Err(WireError::InvalidField);
	}
	Ok(())
}

fn read_amounts(bytes: Vec<u8>) -> Result<Vec<u64>, WireError> {
	if bytes.len() % 8 != 0 {
		return Err(WireError::InvalidTlv(9));
	}
	check_count(bytes.len() / 8)?;
	bytes
		.chunks_exact(8)
		.map(|amount| {
			Ok(u64::from_be_bytes(amount.try_into().map_err(|_| WireError::InvalidTlv(9))?))
		})
		.collect()
}

fn amount_bytes(amounts: &[u64]) -> Vec<u8> {
	amounts.iter().flat_map(|amount| amount.to_be_bytes()).collect()
}

fn read_points(bytes: Vec<u8>) -> Result<Vec<PublicKey>, WireError> {
	let mut reader = Reader::new(&bytes);
	let count = usize::from(reader.u16()?);
	if count > MAX_MESSAGE_LEN / 33 {
		return Err(WireError::SizeLimit);
	}
	let mut peers = Vec::new();
	for _ in 0..count {
		let bytes = reader.take(33)?;
		let point = PublicKey::from_slice(bytes).map_err(|_| WireError::InvalidPoint)?;
		if point.serialize() != bytes {
			return Err(WireError::InvalidPoint);
		}
		peers.push(point);
	}
	if !reader.is_empty() {
		return Err(WireError::InvalidTlv(13));
	}
	Ok(peers)
}
