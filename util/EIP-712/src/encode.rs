// Copyright 2015-2018 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! EIP712 Encoder
use ethabi::{encode, Token as EthAbiToken};
use ethereum_types::{Address as EthAddress, U256, H256};
use keccak_hash::keccak;
use serde_json::Value;
use std::str::FromStr;
use itertools::Itertools;
use linked_hash_set::LinkedHashSet;
use serde_json::to_value;
use parser::{Parser, Type};
use error::{Result, ErrorKind, serde_error};
use eip712::{EIP712, MessageTypes};

/// given a type and HashMap<String, Vec<FieldType>>
/// returns a HashSet of dependent types of the given type
fn build_dependencies<'a>(message_type: &'a str, message_types: &'a MessageTypes) -> Option<(LinkedHashSet<&'a str>)>
{
	if message_types.get(message_type).is_none() {
		return None;
	}

	let mut types = LinkedHashSet::new();
	types.insert(message_type);
	let mut deps = LinkedHashSet::new();

	while let Some(item) = types.pop_back() {
		if let Some(fields) = message_types.get(item) {
			deps.insert(item);

			for field in fields {
				// seen this type before? or not a custom type skip
				if deps.contains(&*field.type_) || !message_types.contains_key(&*field.type_) {
					continue;
				}
				types.insert(&*field.type_);
			}
		}
	};

	return Some(deps)
}

fn encode_type(message_type: &str, message_types: &MessageTypes) -> Result<String> {
	let deps = {
		let mut temp = build_dependencies(message_type, message_types).ok_or_else(|| ErrorKind::NonExistentType)?;
		temp.remove(message_type);
		let mut temp = temp.into_iter().collect::<Vec<_>>();
		(&mut temp[..]).sort_unstable();
		temp.insert(0, message_type);
		temp
	};

	let encoded = deps
		.into_iter()
		.filter_map(|dep| {
			message_types.get(dep).map(|field_types| {
				let types = field_types
					.iter()
					.map(|value| format!("{} {}", value.type_, value.name))
					.join(",");
				return format!("{}({})", dep, types);
			})
		})
		.collect::<Vec<_>>()
		.concat();
	Ok(encoded)
}

fn type_hash(message_type: &str, typed_data: &MessageTypes) -> Result<H256> {
	Ok(keccak(encode_type(message_type, typed_data)?))
}

fn encode_data(
	parser: &Parser,
	message_type: &Type,
	message_types: &MessageTypes,
	value: &Value,
	field_name: Option<&str>
) -> Result<Vec<u8>>
{
	let encoded = match message_type {
		Type::Array(array_type) => {
			let mut items = vec![];

			for item in value.as_array().ok_or_else(|| serde_error("array", field_name))? {
				let mut encoded = encode_data(parser, &*array_type, &message_types, item, field_name)?;
				items.append(&mut encoded);
			}

			keccak(items).0.to_vec()
		}

		Type::Custom(ref ident) if message_types.get(&*ident).is_some() => {
			let type_hash = (&type_hash(ident, &message_types)?).to_vec();
			let mut tokens = encode(&[EthAbiToken::FixedBytes(type_hash)]);

			for field in message_types.get(ident).expect("Already checked in match guard; qed") {
				let value = &value[&field.name];
				let type_ = parser.parse_type(&*field.type_)?;
				let mut encoded = encode_data(parser, &type_, &message_types, &value, Some(&*field.name))?;
				tokens.append(&mut encoded);
			}

			keccak(tokens).0.to_vec()
		}

		Type::Bytes(size) => {
			let string = value.as_str().ok_or_else(|| serde_error("string", field_name))?;
			if string.len() < 2 {
				return Err(ErrorKind::HexParseError(
					format!("Expected a 0x-prefixed string of even length, found {} length string", string.len()))
				)?
			}
			let mut bytes = H256::from_str(&string[2..]).map_err(|err| ErrorKind::HexParseError(format!("{}", err)))?;
			if *size == 32 {
				bytes = keccak(&bytes);
			}
			encode(&[EthAbiToken::FixedBytes(bytes.to_vec())])
		}

		Type::String => {
			let value = value.as_str().ok_or_else(|| serde_error("string", field_name))?;
			let hash = (&keccak(value)).to_vec();
			encode(&[EthAbiToken::FixedBytes(hash)])
		}

		Type::Bool => encode(&[EthAbiToken::Bool(value.as_bool().ok_or_else(|| serde_error("bool", field_name))?)]),

		Type::Address => {
			let addr = value.as_str().ok_or_else(|| serde_error("string", field_name))?;
			if addr.len() != 42 {
				return Err(ErrorKind::InvalidAddressLength(addr.len()))?;
			}
			let address = EthAddress::from_str(&addr[2..]).map_err(|err| ErrorKind::HexParseError(format!("{}", err)))?;
			encode(&[EthAbiToken::Address(address)])
		}

		Type::Uint | Type::Int => {
			// try to deserialize as a number first, then a string
			let uint = match (value.as_u64(), value.as_str()) {
				(Some(number), _) => U256::from(number),
				(_, Some(string)) => {
					if string.len() < 2 {
						return Err(ErrorKind::HexParseError(
							format!("Expected a 0x-prefixed string of even length, found {} length string", string.len()))
						)?
					}
					U256::from_str(&string[2..]).map_err(|err| ErrorKind::HexParseError(format!("{}", err)))?
				}
				_ => return Err(serde_error("int/uint", field_name))?
			};

			let token = if *message_type == Type::Uint {
				EthAbiToken::Uint(uint)
			} else {
				EthAbiToken::Int(uint)
			};
			encode(&[token])
		}

		_ => return Err(ErrorKind::UnknownType("".to_owned(), (*message_type).clone().into()))?
	};

	Ok(encoded)
}

/// encodes and hashes the given EIP712 struct
pub fn hash_structured_data(typed_data: EIP712) -> Result<Vec<u8>> {
	// EIP-191 compliant
	let prefix = (b"\x19\x01").to_vec();
	let domain = to_value(&typed_data.domain).unwrap();
	let parser = Parser::new();
	let (domain_hash, data_hash) = (
		encode_data(&parser, &Type::Custom("EIP712Domain".into()), &typed_data.types, &domain, None)?,
		encode_data(&parser, &Type::Custom(typed_data.primary_type), &typed_data.types, &typed_data.message, None)?
	);
	let concat = [&prefix[..], &domain_hash[..], &data_hash[..]].concat();
	Ok((&keccak(concat)).to_vec())
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::from_str;
	use hex;

	const JSON: &'static str = r#"{
		"primaryType": "Mail",
		"domain": {
			"name": "Ether Mail",
			"version": "1",
			"chainId": "0x1",
			"verifyingContract": "0xCcCCccccCCCCcCCCCCCcCcCccCcCCCcCcccccccC"
		},
		"message": {
			"from": {
				"name": "Cow",
				"wallet": "0xCD2a3d9F938E13CD947Ec05AbC7FE734Df8DD826"
			},
			"to": {
				"name": "Bob",
				"wallet": "0xbBbBBBBbbBBBbbbBbbBbbbbBBbBbbbbBbBbbBBbB"
			},
			"contents": "Hello, Bob!"
		},
		"types": {
			"EIP712Domain": [
			    { "name": "name", "type": "string" },
				{ "name": "version", "type": "string" },
				{ "name": "chainId", "type": "uint256" },
				{ "name": "verifyingContract", "type": "address" }
			],
			"Person": [
				{ "name": "name", "type": "string" },
				{ "name": "wallet", "type": "address" }
			],
			"Mail": [
				{ "name": "from", "type": "Person" },
				{ "name": "to", "type": "Person" },
				{ "name": "contents", "type": "string" }
			]
		}
	}"#;

	#[test]
	fn test_build_dependencies() {
		let string = r#"{
			"EIP712Domain": [
				{ "name": "name", "type": "string" },
				{ "name": "version", "type": "string" },
				{ "name": "chainId", "type": "uint256" },
				{ "name": "verifyingContract", "type": "address" }
			],
			"Person": [
				{ "name": "name", "type": "string" },
				{ "name": "wallet", "type": "address" }
			],
			"Mail": [
				{ "name": "from", "type": "Person" },
				{ "name": "to", "type": "Person" },
				{ "name": "contents", "type": "string" }
			]
		}"#;

		let value = from_str::<MessageTypes>(string).expect("alas error!");
		let mail = "Mail";
		let person = "Person";

		let hashset = {
			let mut temp = LinkedHashSet::new();
			temp.insert(mail);
			temp.insert(person);
			temp
		};
		assert_eq!(build_dependencies(mail, &value), Some(hashset));
	}

	#[test]
	fn test_encode_type() {
		let string = r#"{
			"EIP712Domain": [
				{ "name": "name", "type": "string" },
				{ "name": "version", "type": "string" },
				{ "name": "chainId", "type": "uint256" },
				{ "name": "verifyingContract", "type": "address" }
			],
			"Person": [
				{ "name": "name", "type": "string" },
				{ "name": "wallet", "type": "address" }
			],
			"Mail": [
				{ "name": "from", "type": "Person" },
				{ "name": "to", "type": "Person" },
				{ "name": "contents", "type": "string" }
			]
		}"#;

		let value = from_str::<MessageTypes>(string).expect("alas error!");
		let mail = &String::from("Mail");
		assert_eq!(
			"Mail(Person from,Person to,string contents)Person(string name,address wallet)",
			encode_type(&mail, &value).expect("alas error!")
		)
	}

	#[test]
	fn test_encode_type_hash() {
		let string = r#"{
			"EIP712Domain": [
				{ "name": "name", "type": "string" },
				{ "name": "version", "type": "string" },
				{ "name": "chainId", "type": "uint256" },
				{ "name": "verifyingContract", "type": "address" }
			],
			"Person": [
				{ "name": "name", "type": "string" },
				{ "name": "wallet", "type": "address" }
			],
			"Mail": [
				{ "name": "from", "type": "Person" },
				{ "name": "to", "type": "Person" },
				{ "name": "contents", "type": "string" }
			]
		}"#;

		let value = from_str::<MessageTypes>(string).expect("alas error!");
		let mail = &String::from("Mail");
		let hash = hex::encode(type_hash(&mail, &value).expect("alas error!").0);
		assert_eq!(
			hash,
			"a0cedeb2dc280ba39b857546d74f5549c3a1d7bdc2dd96bf881f76108e23dac2"
		);
	}

	#[test]
	fn test_hash_data() {
		let typed_data = from_str::<EIP712>(JSON).expect("alas error!");
		assert_eq!(
			hex::encode(hash_structured_data(typed_data).expect("alas error!")),
			"be609aee343fb3c4b28e1df9e632fca64fcfaede20f02e86244efddf30957bd2"
		)
	}
}
