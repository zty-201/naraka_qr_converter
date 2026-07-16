use anyhow::Result;
use hyper::Response;
use hyper::body::Bytes;
use serde_json::Value;

use crate::upstream::Upstream;

/// Photo Booth public-detail endpoints we intercept. The game's in-app QR
/// scanner hits `game_detail`; web flows (browsers, curl tests) hit
/// `public_detail`. Both have the same `{code, message, data}` shape and
/// the same `code=30003 share not found` response when the share lives in
/// the other region.
const PHOTO_BOOTH_PATHS: &[&str] = &[
	"/yjwj/studio_share/game_detail",
	"/yjwj/studio_share/public_detail",
];

pub const GLOBAL_API: &str = "api.narakathegame.com";
pub const CN_API: &str = "api.yjwujian.cn";

const GLOBAL_WEB: &str = "www.narakathegame.com";
const CN_WEB: &str = "www.yjwujian.cn";

// Bare domains (no `www.`) — the *original* foreign QR (a WeChat
// mini-program-style link, e.g. `https://yjwujian.cn/xcxtyzz/?...`) uses
// these, unlike the `www.`-prefixed wrapper this proxy rewrites API
// responses into (see `local_share_url` below).
const GLOBAL_DOMAIN: &str = "narakathegame.com";
const CN_DOMAIN: &str = "yjwujian.cn";

fn opposite_region(api_host: &str) -> Option<(&'static str, &'static str)> {
	// (opposite api host, *local* web host — used to rewrite shareUrl so the
	// game thinks the share is local to it).
	match api_host {
		GLOBAL_API => Some((CN_API, GLOBAL_WEB)),
		CN_API => Some((GLOBAL_API, CN_WEB)),
		_ => None,
	}
}

/// Build the wrapper URL the game's client-side pre-check expects for a
/// share it thinks is local to `web_host`. Shared by the proxy rewrite path
/// (below) and the in-app QR converter (`qr` module + `gui`).
pub fn local_share_url(web_host: &str, share_code: &str) -> String {
	format!("https://{web_host}/h5/20260401/yingpengfx/?shareCode={share_code}")
}

/// A foreign-region share decoded from a Photo Booth QR wrapper URL.
pub struct ForeignShare {
	pub share_code: String,
	/// API host that actually holds this shareCode's data (the region the
	/// QR originated from).
	pub origin_api_host: &'static str,
	/// Web host the *opposite* region expects in a rewrapped `shareUrl` —
	/// i.e. what the converted QR's wrapper URL must use, since importing
	/// only makes sense for a share that's foreign to the scanning client.
	pub local_web_host: &'static str,
}

/// Parse the QR a player actually scans out of the game — a WeChat
/// mini-program-style link like
/// `https://yjwujian.cn/xcxtyzz/?activity=...&q=fromH5Game%3D1%26shareCode%3D...`
/// (note: no `www.`, and the shareCode is nested, percent-encoded, inside
/// the `q` param — see [`extract_share_code`]). Resolves which region
/// actually holds the data and which web host a cross-region rewrap must
/// target. Returns `None` if `url` isn't a recognized Naraka Photo Booth
/// share link.
pub fn parse_foreign_wrapper_url(url: &str) -> Option<ForeignShare> {
	let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
	let host = after_scheme.split(['/', '?', '#']).next()?;
	let bare_host = host.strip_prefix("www.").unwrap_or(host);
	let (origin_api_host, local_web_host) = match bare_host {
		GLOBAL_DOMAIN => (GLOBAL_API, CN_WEB),
		CN_DOMAIN => (CN_API, GLOBAL_WEB),
		_ => return None,
	};
	let share_code = extract_share_code(after_scheme)?;
	Some(ForeignShare { share_code, origin_api_host, local_web_host })
}

/// If `resp` is a "share not found" response for a Photo Booth public_detail
/// lookup, try the other region's API and patch the JSON `data` field in place
/// (rewriting `shareUrl` so the game thinks it came from its own region).
pub async fn maybe_rewrite_photo_booth(
	host: &str,
	req_path: &str,
	upstream: &Upstream,
	resp: Response<Bytes>,
) -> Result<Response<Bytes>> {
	let Some(matched_path) = PHOTO_BOOTH_PATHS
		.iter()
		.copied()
		.find(|p| req_path.starts_with(p))
	else {
		return Ok(resp);
	};
	let Some((opposite_api, local_web)) = opposite_region(host) else {
		return Ok(resp);
	};
	let Some(share_code) = extract_share_code(req_path) else {
		return Ok(resp);
	};
	let Ok(mut json) = serde_json::from_slice::<Value>(resp.body()) else {
		return Ok(resp);
	};
	if json.get("code").and_then(Value::as_i64).unwrap_or(0) == 0 {
		return Ok(resp);
	}

	// Fall back to the same endpoint on the opposite region so the response
	// shape matches what the game is parsing on this side.
	let opposite_url = format!("https://{opposite_api}{matched_path}?shareCode={share_code}");
	tracing::info!(host, opposite_api, path = matched_path, share_code, "share not found locally; trying opposite region");

	let opposite_json = match upstream.get_json(&opposite_url).await {
		Ok(v) => v,
		Err(err) => {
			tracing::warn!(?err, "opposite-region lookup failed");
			return Ok(resp);
		}
	};

	if opposite_json.get("code").and_then(Value::as_i64).unwrap_or(-1) != 0 {
		tracing::info!("share also not found on opposite region");
		return Ok(resp);
	}

	let (mut parts, _body) = resp.into_parts();

	// Found on the opposite region. Splice its `data` into our response and
	// rewrite shareUrl so the local game thinks the share is local.
	let mut data = opposite_json.get("data").cloned().unwrap_or(Value::Null);
	if let Some(obj) = data.as_object_mut() {
		obj.insert("shareUrl".to_string(), Value::String(local_share_url(local_web, &share_code)));
	}

	json["code"] = Value::from(0);
	json["message"] = Value::String("Success".into());
	json["data"] = data;

	let new_body = serde_json::to_vec(&json)?;
	parts.status = hyper::StatusCode::OK;
	parts.headers.remove("content-length");
	parts.headers.remove("content-encoding");
	parts
		.headers
		.insert("content-type", "application/json; charset=utf-8".parse()?);

	tracing::info!(host, share_code, "rewrote response with opposite-region data");
	Ok(Response::from_parts(parts, Bytes::from(new_body)))
}

/// Finds `shareCode` either as a top-level query param (how the game's own
/// API calls look) or nested inside a percent-encoded `q` param (how the
/// mini-program-style QR the game generates looks — its `q` value is itself
/// a `key=value&key=value` string, just percent-encoded once more).
fn extract_share_code(path_and_query: &str) -> Option<String> {
	let query = path_and_query.split_once('?').map(|(_, q)| q)?;
	for pair in query.split('&') {
		if let Some(value) = pair.strip_prefix("shareCode=") {
			return Some(urldecode(value));
		}
		if let Some(value) = pair.strip_prefix("q=") {
			let decoded = urldecode(value);
			for inner in decoded.split('&') {
				if let Some(share_code) = inner.strip_prefix("shareCode=") {
					return Some(urldecode(share_code));
				}
			}
		}
	}
	None
}

fn urldecode(s: &str) -> String {
	// Decode into a byte buffer first: `%E4%BD%A0` is a single UTF-8 codepoint,
	// not three Latin-1 chars, so we can't push each byte as `char` directly.
	let mut out: Vec<u8> = Vec::with_capacity(s.len());
	let bytes = s.as_bytes();
	let mut i = 0;
	while i < bytes.len() {
		match bytes[i] {
			b'%' if i + 2 < bytes.len() => {
				if let (Some(d1), Some(d2)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
					out.push(d1 * 16 + d2);
					i += 3;
				} else {
					out.push(b'%');
					i += 1;
				}
			}
			b'+' => { out.push(b' '); i += 1; }
			b => { out.push(b); i += 1; }
		}
	}
	String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

fn hex_val(b: u8) -> Option<u8> {
	match b {
		b'0'..=b'9' => Some(b - b'0'),
		b'a'..=b'f' => Some(b - b'a' + 10),
		b'A'..=b'F' => Some(b - b'A' + 10),
		_ => None,
	}
}
