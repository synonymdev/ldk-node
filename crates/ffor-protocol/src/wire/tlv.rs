use super::{Tlv, WireError, MAX_MESSAGE_LEN, MAX_TLV_COUNT};

pub(super) struct Reader<'a> {
	remaining: &'a [u8],
}

impl<'a> Reader<'a> {
	pub(super) fn new(bytes: &'a [u8]) -> Self {
		Self { remaining: bytes }
	}

	pub(super) fn take(&mut self, size: usize) -> Result<&'a [u8], WireError> {
		let value = self.remaining.get(..size).ok_or(WireError::Truncated)?;
		self.remaining = &self.remaining[size..];
		Ok(value)
	}

	pub(super) fn array<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
		self.take(N)?.try_into().map_err(|_| WireError::Truncated)
	}

	pub(super) fn u8(&mut self) -> Result<u8, WireError> {
		Ok(self.array::<1>()?[0])
	}
	pub(super) fn u16(&mut self) -> Result<u16, WireError> {
		Ok(u16::from_be_bytes(self.array()?))
	}
	pub(super) fn u32(&mut self) -> Result<u32, WireError> {
		Ok(u32::from_be_bytes(self.array()?))
	}
	pub(super) fn u64(&mut self) -> Result<u64, WireError> {
		Ok(u64::from_be_bytes(self.array()?))
	}
	pub(super) fn is_empty(&self) -> bool {
		self.remaining.is_empty()
	}

	fn bigsize(&mut self) -> Result<u64, WireError> {
		let (value, minimum) = match self.u8()? {
			0xfd => (u64::from(self.u16()?), 0xfd),
			0xfe => (u64::from(self.u32()?), 0x1_0000),
			0xff => (self.u64()?, 0x1_0000_0000),
			value => return Ok(u64::from(value)),
		};
		if value < minimum {
			return Err(WireError::NonCanonicalBigSize);
		}
		Ok(value)
	}
}

pub(super) fn read_tlvs(reader: &mut Reader<'_>) -> Result<Vec<Tlv>, WireError> {
	let mut entries = Vec::new();
	let mut last = None;
	while !reader.is_empty() {
		let kind = reader.bigsize()?;
		if last.is_some_and(|previous| kind <= previous) {
			return Err(WireError::TlvOrder);
		}
		let len = usize::try_from(reader.bigsize()?).map_err(|_| WireError::SizeLimit)?;
		if len > MAX_MESSAGE_LEN || entries.len() >= MAX_TLV_COUNT {
			return Err(WireError::SizeLimit);
		}
		let value = reader.take(len)?.to_vec();
		entries.push(Tlv { kind, value });
		last = Some(kind);
	}
	Ok(entries)
}

pub(super) fn required(entries: &mut Vec<Tlv>, kind: u64) -> Result<Vec<u8>, WireError> {
	optional(entries, kind).ok_or(WireError::MissingTlv(kind))
}

pub(super) fn optional(entries: &mut Vec<Tlv>, kind: u64) -> Option<Vec<u8>> {
	let position = entries.iter().position(|entry| entry.kind == kind)?;
	Some(entries.remove(position).value)
}

pub(super) fn check_extensions(entries: &[Tlv]) -> Result<(), WireError> {
	if entries.len() > MAX_TLV_COUNT {
		return Err(WireError::SizeLimit);
	}
	let mut previous = None;
	let mut total = 0usize;
	for entry in entries {
		if previous.is_some_and(|last| entry.kind <= last) {
			return Err(WireError::TlvOrder);
		}
		if entry.kind % 2 == 0 {
			return Err(WireError::UnknownEvenTlv(entry.kind));
		}
		total = total
			.checked_add(entry.value.len())
			.and_then(|n| n.checked_add(2))
			.ok_or(WireError::SizeLimit)?;
		if total > MAX_MESSAGE_LEN {
			return Err(WireError::SizeLimit);
		}
		previous = Some(entry.kind);
	}
	Ok(())
}

pub(super) struct Writer(pub(super) Vec<u8>);

impl Writer {
	pub(super) fn new() -> Self {
		Self(Vec::new())
	}
	pub(super) fn put(&mut self, bytes: &[u8]) -> Result<(), WireError> {
		if bytes.len() > MAX_MESSAGE_LEN - self.0.len() {
			return Err(WireError::SizeLimit);
		}
		self.0.extend_from_slice(bytes);
		Ok(())
	}
	pub(super) fn u16(&mut self, value: u16) -> Result<(), WireError> {
		self.put(&value.to_be_bytes())
	}
	pub(super) fn u32(&mut self, value: u32) -> Result<(), WireError> {
		self.put(&value.to_be_bytes())
	}
	pub(super) fn u64(&mut self, value: u64) -> Result<(), WireError> {
		self.put(&value.to_be_bytes())
	}

	fn bigsize(&mut self, value: u64) -> Result<(), WireError> {
		match value {
			0..=0xfc => self.put(&[value as u8]),
			0xfd..=0xffff => {
				self.put(&[0xfd])?;
				self.u16(value as u16)
			},
			0x1_0000..=0xffff_ffff => {
				self.put(&[0xfe])?;
				self.u32(value as u32)
			},
			_ => {
				self.put(&[0xff])?;
				self.u64(value)
			},
		}
	}
}

pub(super) fn write_tlvs(
	writer: &mut Writer, known: Vec<Tlv>, extensions: &[Tlv],
) -> Result<(), WireError> {
	check_extensions(extensions)?;
	for extension in extensions {
		if known.iter().any(|tlv| tlv.kind == extension.kind) {
			return Err(WireError::InvalidTlv(extension.kind));
		}
	}
	let mut entries: Vec<_> = known.iter().chain(extensions).collect();
	entries.sort_by_key(|entry| entry.kind);
	for entry in entries {
		writer.bigsize(entry.kind)?;
		writer.bigsize(entry.value.len() as u64)?;
		writer.put(&entry.value)?;
	}
	Ok(())
}

pub(super) fn fixed<const N: usize>(value: Vec<u8>, kind: u64) -> Result<[u8; N], WireError> {
	value.try_into().map_err(|_| WireError::InvalidTlv(kind))
}
