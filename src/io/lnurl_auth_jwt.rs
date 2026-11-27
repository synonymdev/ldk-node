// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! LNURL-auth JWT header provider that signs for a fixed key domain.
//!
//! Port of vss-client-ng 0.4.1 `src/headers/lnurl_auth_jwt.rs`. Upstream derives the LUD-05
//! linking key from the domain of the LNURL the auth server returns. This provider derives it
//! from a configured key domain instead and sends the signed callback to the host of the
//! configured auth URL, so the auth server can move hosts while wallets keep the same linking
//! key (the JWT `sub` that VSS stores data under).

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use bitcoin::bip32::{ChildNumber, DerivationPath, Xpriv};
use bitcoin::hashes::hex::FromHex;
use bitcoin::hashes::{sha256, Hash, HashEngine, Hmac, HmacEngine};
use bitcoin::secp256k1::{Message, Secp256k1, SignOnly};
use bitcoin::PrivateKey;
use serde::Deserialize;
use url::Url;
use vss_client::headers::{VssHeaderProvider, VssHeaderProviderError};

// Derivation index of the hashing private key as defined by LUD-05.
const HASHING_DERIVATION_INDEX: u32 = 0;
// The JWT token will be refreshed by the given amount before its expiry.
const EXPIRY_BUFFER: Duration = Duration::from_secs(60);
const K1_QUERY_PARAM: &str = "k1";
const SIG_QUERY_PARAM: &str = "sig";
const KEY_QUERY_PARAM: &str = "key";
const AUTHORIZATION: &str = "Authorization";

#[derive(Debug, Clone)]
struct JwtToken {
	token_str: String,
	expiry: Option<SystemTime>,
}

impl JwtToken {
	fn is_expired(&self) -> bool {
		self.expiry
			.and_then(|expiry| {
				SystemTime::now()
					.checked_add(EXPIRY_BUFFER)
					.map(|now_with_buffer| now_with_buffer > expiry)
			})
			.unwrap_or(false)
	}
}

pub(crate) struct KeyDomainLnurlAuthProvider {
	engine: Secp256k1<SignOnly>,
	parent_key: Xpriv,
	url: Url,
	key_domain: String,
	default_headers: HashMap<String, String>,
	client: reqwest::Client,
	cached_jwt_token: RwLock<Option<JwtToken>>,
}

impl KeyDomainLnurlAuthProvider {
	/// Creates a provider that signs for `key_domain` and calls back on `url`'s host.
	///
	/// `default_headers` are sent with the LNURL requests and returned with the JWT
	/// authorization header for VSS requests. Rejects an auth URL whose host is neither
	/// `key_domain` nor one of its subdomains, so the linking key never signs a challenge for
	/// a host outside its domain.
	pub(crate) fn new(
		parent_key: Xpriv, url: String, key_domain: String,
		default_headers: HashMap<String, String>,
	) -> Result<Self, VssHeaderProviderError> {
		Self::new_with_host_check(parent_key, url, key_domain, default_headers, true)
	}

	fn new_with_host_check(
		parent_key: Xpriv, url: String, key_domain: String,
		default_headers: HashMap<String, String>, check_host: bool,
	) -> Result<Self, VssHeaderProviderError> {
		let url = Url::parse(&url).map_err(|e| VssHeaderProviderError::InvalidData {
			error: format!("invalid LNURL-auth server URL: {}", e),
		})?;
		// Canonicalize as `Url::domain()` does for the upstream key domain.
		let key_domain = match url::Host::parse(&key_domain) {
			Ok(url::Host::Domain(domain)) => domain,
			_ => {
				return Err(VssHeaderProviderError::InvalidData {
					error: format!("invalid LNURL-auth key domain: {}", key_domain.escape_debug()),
				})
			},
		};
		let host = url.host_str().unwrap_or_default();
		if check_host && host != key_domain && !host.ends_with(&format!(".{key_domain}")) {
			return Err(VssHeaderProviderError::InvalidData {
				error: format!(
					"LNURL-auth host {} is outside key domain {}",
					host.escape_debug(),
					key_domain
				),
			});
		}
		let mut default_headermap = reqwest::header::HeaderMap::new();
		for (name, value) in &default_headers {
			let invalid = || VssHeaderProviderError::InvalidData {
				error: format!("invalid header: {}", name.escape_debug()),
			};
			default_headermap.insert(
				reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid())?,
				reqwest::header::HeaderValue::from_str(value).map_err(|_| invalid())?,
			);
		}
		let client = reqwest::Client::builder()
			.default_headers(default_headermap)
			.build()
			.map_err(|e| VssHeaderProviderError::RequestError { error: e.to_string() })?;
		Ok(Self {
			engine: Secp256k1::signing_only(),
			parent_key,
			url,
			key_domain,
			default_headers,
			client,
			cached_jwt_token: RwLock::new(None),
		})
	}

	async fn fetch_jwt_token(&self) -> Result<JwtToken, VssHeaderProviderError> {
		let request_error =
			|e: reqwest::Error| VssHeaderProviderError::RequestError { error: e.to_string() };
		let lnurl_str = self
			.client
			.get(self.url.clone())
			.send()
			.await
			.map_err(request_error)?
			.text()
			.await
			.map_err(request_error)?;

		let signed_lnurl =
			sign_lnurl(&self.engine, &self.parent_key, &lnurl_str, &self.key_domain, &self.url)?;
		let lnurl_auth_body = self
			.client
			.get(&signed_lnurl)
			.send()
			.await
			.map_err(request_error)?
			.bytes()
			.await
			.map_err(request_error)?;
		let lnurl_auth_response: LnurlAuthResponse = serde_json::from_slice(&lnurl_auth_body)
			.map_err(|e| VssHeaderProviderError::InvalidData {
				error: format!("invalid LNURL Auth response: {}", e),
			})?;

		let untrusted_token = match lnurl_auth_response {
			LnurlAuthResponse { token: Some(token), .. } => token,
			LnurlAuthResponse { reason: Some(reason), .. } => {
				return Err(VssHeaderProviderError::AuthorizationError {
					error: format!("LNURL Auth failed, reason is: {}", reason.escape_debug()),
				});
			},
			_ => {
				return Err(VssHeaderProviderError::InvalidData {
					error: "LNURL Auth response did not contain a token nor an error".to_string(),
				});
			},
		};
		parse_jwt_token(untrusted_token)
	}

	async fn get_jwt_token(&self) -> Result<String, VssHeaderProviderError> {
		let cached_token_str = {
			let jwt_token = self.cached_jwt_token.read().unwrap();
			jwt_token.as_ref().filter(|t| !t.is_expired()).map(|t| t.token_str.clone())
		};
		if let Some(token_str) = cached_token_str {
			return Ok(token_str);
		}
		let jwt_token = self.fetch_jwt_token().await?;
		*self.cached_jwt_token.write().unwrap() = Some(jwt_token.clone());
		Ok(jwt_token.token_str)
	}
}

#[async_trait]
impl VssHeaderProvider for KeyDomainLnurlAuthProvider {
	async fn get_headers(
		&self, _request: &[u8],
	) -> Result<HashMap<String, String>, VssHeaderProviderError> {
		let jwt_token = self.get_jwt_token().await?;
		let mut headers = self.default_headers.clone();
		headers.insert(AUTHORIZATION.to_string(), format!("Bearer {}", jwt_token));
		Ok(headers)
	}
}

fn hashing_key(
	engine: &Secp256k1<SignOnly>, parent_key: &Xpriv,
) -> Result<PrivateKey, VssHeaderProviderError> {
	let hashing_child_number =
		ChildNumber::from_normal_idx(HASHING_DERIVATION_INDEX).map_err(internal_error)?;
	parent_key
		.derive_priv(engine, &[hashing_child_number])
		.map(|xpriv| xpriv.to_priv())
		.map_err(internal_error)
}

fn linking_key_path(hashing_key: &PrivateKey, domain_name: &str) -> DerivationPath {
	let mut engine = HmacEngine::<sha256::Hash>::new(&hashing_key.inner[..]);
	engine.input(domain_name.as_bytes());
	let result = Hmac::<sha256::Hash>::from_engine(engine).to_byte_array();
	// unwrap safety: We take 4-byte chunks, so TryInto for [u8; 4] never fails.
	let children = result
		.chunks_exact(4)
		.take(4)
		.map(|i| u32::from_be_bytes(i.try_into().unwrap()))
		.map(ChildNumber::from);
	DerivationPath::from_iter(children)
}

/// Signs `lnurl_str` with the linking key for `key_domain` and points the callback at
/// `auth_url`'s scheme, host and port, keeping its path and query.
fn sign_lnurl(
	engine: &Secp256k1<SignOnly>, parent_key: &Xpriv, lnurl_str: &str, key_domain: &str,
	auth_url: &Url,
) -> Result<String, VssHeaderProviderError> {
	let invalid_lnurl = || VssHeaderProviderError::InvalidData {
		error: format!("invalid lnurl: {}", lnurl_str.escape_debug()),
	};
	let mut lnurl = Url::parse(lnurl_str).map_err(|_| invalid_lnurl())?;
	let k1_str = lnurl
		.query_pairs()
		.find(|(k, _)| k == K1_QUERY_PARAM)
		.ok_or_else(invalid_lnurl)?
		.1
		.to_string();
	let k1: [u8; 32] = FromHex::from_hex(&k1_str).map_err(|_| invalid_lnurl())?;

	let hashing_private_key = hashing_key(engine, parent_key)?;
	let linking_key_path = linking_key_path(&hashing_private_key, key_domain);
	let linking_private_key =
		parent_key.derive_priv(engine, &linking_key_path).map_err(internal_error)?.to_priv();
	let linking_public_key = linking_private_key.public_key(engine);
	let message = Message::from_digest(k1);
	let sig = engine.sign_ecdsa(&message, &linking_private_key.inner);

	lnurl.set_scheme(auth_url.scheme()).map_err(|_| invalid_lnurl())?;
	lnurl.set_host(auth_url.host_str()).map_err(|_| invalid_lnurl())?;
	lnurl.set_port(auth_url.port()).map_err(|_| invalid_lnurl())?;
	lnurl
		.query_pairs_mut()
		.append_pair(SIG_QUERY_PARAM, &sig.serialize_der().to_string())
		.append_pair(KEY_QUERY_PARAM, &linking_public_key.to_string());
	Ok(lnurl.to_string())
}

fn internal_error(e: bitcoin::bip32::Error) -> VssHeaderProviderError {
	VssHeaderProviderError::InternalError { error: e.to_string() }
}

#[derive(Deserialize, Debug, Clone)]
struct LnurlAuthResponse {
	reason: Option<String>,
	token: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct ExpiryClaim {
	#[serde(rename = "exp")]
	expiry_secs: Option<u64>,
}

fn parse_jwt_token(jwt_token: String) -> Result<JwtToken, VssHeaderProviderError> {
	let parts: Vec<&str> = jwt_token.split('.').collect();
	let invalid = || VssHeaderProviderError::InvalidData {
		error: format!("invalid JWT token: {}", jwt_token.escape_debug()),
	};
	if parts.len() != 3 {
		return Err(invalid());
	}
	let _ = URL_SAFE_NO_PAD.decode(parts[0]).map_err(|_| invalid())?;
	let bytes = URL_SAFE_NO_PAD.decode(parts[1]).map_err(|_| invalid())?;
	let _ = URL_SAFE_NO_PAD.decode(parts[2]).map_err(|_| invalid())?;
	let claim: ExpiryClaim = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
	let expiry =
		claim.expiry_secs.and_then(|e| SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(e)));
	Ok(JwtToken { token_str: jwt_token, expiry })
}

#[cfg(test)]
mod tests {
	use std::io::{BufRead, BufReader, Write};
	use std::net::{SocketAddr, TcpListener};
	use std::sync::{Arc, Mutex};

	use bitcoin::Network;

	use super::*;

	const K1: &str = "e2af6254a8df433264fa23f67eb8188635d15ce883e8fc020989d5f82ae6f11e";

	fn upstream_test_parent_key() -> Xpriv {
		let bytes: [u8; 32] =
			FromHex::from_hex("abababababababababababababababababababababababababababababababab")
				.unwrap();
		Xpriv::new_master(Network::Testnet, &bytes).unwrap()
	}

	fn sign(lnurl: &str, key_domain: &str, auth_url: &str) -> String {
		sign_lnurl(
			&Secp256k1::signing_only(),
			&upstream_test_parent_key(),
			lnurl,
			key_domain,
			&Url::parse(auth_url).unwrap(),
		)
		.unwrap()
	}

	// vss-client-ng 0.4.1's `sign_lnurl` with the same parent key, for the `bitkit.to`
	// LNURL the mainnet auth server returns.
	#[test]
	fn signs_for_bitkit_to_on_api_bitkit_to() {
		let signed = sign(
			&format!("https://bitkit.to/lnurl_auth/auth?tag=login&k1={K1}&action=login"),
			"bitkit.to",
			"https://api.bitkit.to/lnurl_auth/auth",
		);
		assert_eq!(
			signed,
			format!("https://api.bitkit.to/lnurl_auth/auth?tag=login&k1={K1}&action=login&sig=3045022100b8924431a127bbad6ebe3ce7f9f1894907c1087395306ef1f6a2b7cdcb63fc9f022067c20224274f743c70b8465619369d06742ccc7a88f3d2def14d79dd4c1ed346&key=03a758e690253eb7dace3ee0f351da6fb54f15873cc767dfc85d616e2d9f498986"),
		);
	}

	// The key domain decides the key, whatever host the returned LNURL names.
	#[test]
	fn signs_for_key_domain_when_lnurl_host_differs() {
		let signed = sign(
			&format!("https://api.bitkit.to/lnurl_auth/auth?tag=login&k1={K1}&action=login"),
			"bitkit.to",
			"https://api.bitkit.to/lnurl_auth/auth",
		);
		assert!(signed
			.ends_with("&key=03a758e690253eb7dace3ee0f351da6fb54f15873cc767dfc85d616e2d9f498986"));
	}

	#[test]
	fn canonicalizes_key_domain() {
		let new = |key_domain: &str| {
			KeyDomainLnurlAuthProvider::new(
				upstream_test_parent_key(),
				"https://api.bitkit.to/lnurl_auth/auth".to_string(),
				key_domain.to_string(),
				HashMap::new(),
			)
		};
		assert_eq!(new("BITKIT.TO").unwrap().key_domain, "bitkit.to");
		assert!(new("").is_err());
		assert!(new("127.0.0.1").is_err());
		assert!(new("https://bitkit.to").is_err());
	}

	#[test]
	fn rejects_auth_host_outside_key_domain() {
		let new = |url: &str| {
			KeyDomainLnurlAuthProvider::new(
				upstream_test_parent_key(),
				url.to_string(),
				"bitkit.to".to_string(),
				HashMap::new(),
			)
		};
		assert!(new("https://bitkit.to/lnurl_auth/auth").is_ok());
		assert!(new("https://api.bitkit.to/lnurl_auth/auth").is_ok());
		for url in [
			"https://evil.example/lnurl_auth/auth",
			"https://notbitkit.to/lnurl_auth/auth",
			"https://bitkit.to.evil.example/lnurl_auth/auth",
			"http://127.0.0.1:8080/lnurl_auth/auth",
		] {
			assert!(matches!(new(url), Err(VssHeaderProviderError::InvalidData { .. })), "{url}");
		}
	}

	// vss-client-ng 0.4.1's published `test_sign_lnurl` vector, signed for `example.com`
	// while the callback moves to another host.
	#[test]
	fn matches_upstream_vector_on_another_host() {
		let signed = sign(
			&format!("https://example.com/path?tag=login&k1={K1}"),
			"example.com",
			"http://127.0.0.1:8080/lnurl",
		);
		assert_eq!(
			signed,
			format!("http://127.0.0.1:8080/path?tag=login&k1={K1}&sig=3045022100a75df468de452e618edb8030016eb0894204655c7d93ece1be007fcf36843522022048bc2f00a0a5a30601d274b49cfaf9ef4c76176e5401d0dfb195f5d6ab8ab4c4&key=02d9eb1b467517d685e3b5439082c14bb1a2c9ae672df4d9046d208c193a5846e0"),
		);
	}

	struct AuthServer {
		addr: SocketAddr,
		requests: Arc<Mutex<Vec<String>>>,
	}

	impl AuthServer {
		fn paths(&self) -> Vec<String> {
			self.requests.lock().unwrap().iter().filter(|r| r.starts_with('/')).cloned().collect()
		}
	}

	// Serves the mainnet challenge shape on loopback and answers the n-th signed callback with
	// `callback_body(n)`. Records each request path and each header line, lowercased.
	fn serve_auth(callback_body: impl Fn(usize) -> String + Send + 'static) -> AuthServer {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let addr = listener.local_addr().unwrap();
		let requests = Arc::new(Mutex::new(Vec::new()));
		let server_requests = Arc::clone(&requests);
		std::thread::spawn(move || {
			let mut callbacks = 0;
			for stream in listener.incoming() {
				let mut stream = stream.unwrap();
				let mut reader = BufReader::new(stream.try_clone().unwrap());
				let mut request_line = String::new();
				reader.read_line(&mut request_line).unwrap();
				loop {
					let mut line = String::new();
					reader.read_line(&mut line).unwrap();
					if line == "\r\n" || line.is_empty() {
						break;
					}
					server_requests.lock().unwrap().push(line.trim_end().to_ascii_lowercase());
				}
				let path = request_line.split(' ').nth(1).unwrap().to_string();
				server_requests.lock().unwrap().push(path.clone());
				let body = if path.contains("sig=") {
					callbacks += 1;
					callback_body(callbacks - 1)
				} else {
					format!("https://bitkit.to/lnurl_auth/auth?tag=login&k1={K1}&action=login")
				};
				write!(
					stream,
					"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
					body.len(),
					body
				)
				.unwrap();
			}
		});
		AuthServer { addr, requests }
	}

	fn jwt(exp: u64) -> String {
		let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"sub":"x","exp":{exp}}}"#));
		format!("e30.{payload}.c2ln")
	}

	fn token_body(token: String) -> String {
		format!(r#"{{"token":"{token}"}}"#)
	}

	// Loopback is outside `bitkit.to`, so these tests skip the host check.
	fn provider(server: &AuthServer) -> KeyDomainLnurlAuthProvider {
		KeyDomainLnurlAuthProvider::new_with_host_check(
			upstream_test_parent_key(),
			format!("http://{}/lnurl_auth/auth", server.addr),
			"bitkit.to".to_string(),
			HashMap::from([("X-Test".to_string(), "1".to_string())]),
			false,
		)
		.unwrap()
	}

	// The callback reaches the auth host with the bitkit.to key, and the JWT is reused while
	// unexpired.
	#[tokio::test]
	async fn fetches_and_caches_jwt_via_auth_host() {
		let server = serve_auth(|_| token_body(jwt(4102444800)));
		let provider = provider(&server);
		for _ in 0..2 {
			let headers = provider.get_headers(&[]).await.unwrap();
			assert_eq!(headers.get("Authorization"), Some(&format!("Bearer {}", jwt(4102444800))));
			assert_eq!(headers.get("X-Test").map(String::as_str), Some("1"));
		}

		let paths = server.paths();
		assert_eq!(paths.len(), 2, "the second call reuses the cached JWT");
		assert_eq!(paths[0], "/lnurl_auth/auth");
		assert!(
			paths[1].starts_with(&format!("/lnurl_auth/auth?tag=login&k1={K1}&action=login&sig="))
		);
		assert!(paths[1]
			.ends_with("&key=03a758e690253eb7dace3ee0f351da6fb54f15873cc767dfc85d616e2d9f498986"));
		assert!(server.requests.lock().unwrap().contains(&format!("host: {}", server.addr)));
	}

	#[tokio::test]
	async fn refetches_expired_jwt() {
		let server = serve_auth(|n| token_body(jwt(1 + n as u64)));
		let provider = provider(&server);
		let first = provider.get_headers(&[]).await.unwrap();
		let second = provider.get_headers(&[]).await.unwrap();
		assert_eq!(server.paths().len(), 4, "an expired JWT is fetched again");
		assert_ne!(first.get("Authorization"), second.get("Authorization"));
	}

	#[tokio::test]
	async fn reports_auth_server_reason() {
		let server =
			serve_auth(|_| r#"{"status":"ERROR","reason":"Invalid signature"}"#.to_string());
		let err = provider(&server).get_headers(&[]).await.unwrap_err();
		assert!(
			matches!(&err, VssHeaderProviderError::AuthorizationError { error } if error.contains("Invalid signature")),
			"{err:?}"
		);
	}

	#[tokio::test]
	async fn rejects_malformed_jwt() {
		let server = serve_auth(|_| token_body("not-a-jwt".to_string()));
		let err = provider(&server).get_headers(&[]).await.unwrap_err();
		assert!(matches!(err, VssHeaderProviderError::InvalidData { .. }), "{err:?}");
	}
}
