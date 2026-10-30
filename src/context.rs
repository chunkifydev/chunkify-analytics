use axum::http::HeaderMap;
use ipnet::IpNet;
use serde_json::{Map, Value, json};
use std::net::IpAddr;

const MAX_FORWARDED_BYTES: usize = 4096;
const MAX_FORWARDED_HOPS: usize = 32;

pub fn trusted_proxies(value: &str) -> Result<Vec<IpNet>, &'static str> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|entry| {
            let entry = entry.trim();
            if let Ok(ip) = entry.parse::<IpAddr>() {
                return Ok(IpNet::from(ip.to_canonical()));
            }
            let network = entry.parse::<IpNet>().map_err(
                |_| "server.trusted_proxies must contain comma-separated IP addresses or CIDRs",
            )?;
            if matches!(network, IpNet::V6(net) if net.addr().to_ipv4_mapped().is_some()) {
                return Err("server.trusted_proxies must use IPv4 CIDRs for IPv4-mapped addresses");
            }
            Ok(network)
        })
        .collect()
}

/// Resolve the viewer address only for the in-memory geolocation lookup.
/// Never pass IP addresses to the writer or attach them to event metadata.
pub fn client_ip(peer: IpAddr, headers: &HeaderMap, trusted: &[IpNet]) -> Option<IpAddr> {
    let peer = peer.to_canonical();
    let is_trusted = |ip: &IpAddr| trusted.iter().any(|network| network.contains(ip));
    if is_trusted(&peer) {
        // Missing or invalid forwarding data must not label a proxy as a viewer.
        forwarded_chain(headers)
            .and_then(|chain| chain.into_iter().rev().find(|ip| !is_trusted(ip)))
    } else {
        Some(peer)
    }
}

/// Copy only the supported browser fields. Request fields can only come from
/// the collector; the original event still preserves all caller-supplied data.
pub fn event_context(event: &Value, request: Option<&Value>) -> Option<Value> {
    let mut context = event_context_v1(event, request)
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if let Some(browser) = event.get("browser_context").and_then(Value::as_object)
        && browser.get("schema_version") == Some(&json!(1))
    {
        for key in ["page_origin", "page_path"] {
            if let Some(value) = browser.get(key)
                && (value.is_string() || value.is_null())
            {
                context.insert(key.into(), value.clone());
            }
        }
    }
    if context.is_empty() {
        return None;
    }
    context.insert("schema_version".into(), json!(1));
    Some(Value::Object(context))
}

// Frozen for schema-v4 reconstruction. Introduce a new storage context format
// before changing these rules, so historical responses remain unchanged.
pub(crate) fn event_context_v1(event: &Value, request: Option<&Value>) -> Option<Value> {
    let mut context = Map::new();
    if let Some(browser) = event.get("browser_context").and_then(Value::as_object)
        && browser.get("schema_version") == Some(&json!(1))
    {
        for key in [
            "user_agent",
            "browser_name",
            "browser_version",
            "os_name",
            "os_version",
            "device_type",
            "device_family",
            "language",
            "language_prefix",
            "timezone",
            "timezone_offset_minutes",
            "screen_width_px",
            "screen_height_px",
            "viewport_width_px",
            "viewport_height_px",
            "pixel_ratio",
        ] {
            if let Some(value) = browser.get(key)
                && !value.is_object()
                && !value.is_array()
            {
                context.insert(key.into(), value.clone());
            }
        }
    }
    if let Some(request) = request.and_then(Value::as_object) {
        context.extend(request.clone());
    }
    if context.is_empty() {
        return None;
    }
    context.insert("schema_version".into(), json!(1));
    Some(Value::Object(context))
}

fn forwarded_chain(headers: &HeaderMap) -> Option<Vec<IpAddr>> {
    let mut chain = Vec::new();
    let mut bytes = 0;
    // Preserve order across all header lines, as for a single comma-separated list.
    for header in headers.get_all("x-forwarded-for") {
        bytes += header.as_bytes().len();
        if bytes > MAX_FORWARDED_BYTES {
            return None;
        }
        for value in header.to_str().ok()?.split(',') {
            if chain.len() == MAX_FORWARDED_HOPS {
                return None;
            }
            chain.push(value.trim().parse::<IpAddr>().ok()?.to_canonical());
        }
    }
    Some(chain)
}
