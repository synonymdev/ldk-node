use bitcoin::secp256k1::PublicKey;
use ffor_protocol::wire::Message;
use serde::Deserialize;

#[derive(Deserialize)]
struct Reference {
	source_revision: String,
	fixtures: Vec<Fixture>,
}

#[derive(Deserialize)]
struct Fixture {
	name: String,
	message_type: u16,
	signer: String,
	wire: String,
}

fn hex(value: &str) -> Vec<u8> {
	(0..value.len()).step_by(2).map(|i| u8::from_str_radix(&value[i..i + 2], 16).unwrap()).collect()
}

#[test]
fn beignet_lifecycle_messages_preserve_signed_wire_bytes() {
	let reference: Reference =
		serde_json::from_str(include_str!("data/beignet-lifecycle.json")).unwrap();
	assert_eq!(reference.source_revision, "8aee31d18e596fe49a0d195b325a6e757d7a009b");
	assert_eq!(reference.fixtures.len(), 5);
	for fixture in reference.fixtures {
		let wire = hex(&fixture.wire);
		let decoded =
			Message::decode(&wire).unwrap_or_else(|error| panic!("{}: {error}", fixture.name));
		assert_eq!(decoded.message_type(), fixture.message_type);
		decoded.verify_signature(&PublicKey::from_slice(&hex(&fixture.signer)).unwrap()).unwrap();
		assert_eq!(decoded.encode().unwrap(), wire, "{}", fixture.name);
	}
}
