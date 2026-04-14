//! WiZ smart bulb UDP control.
//!
//! WiZ bulbs speak a small JSON-over-UDP protocol on port 38899:
//! - Discovery is a broadcast `registration` request, bulbs reply with their MAC.
//! - `setPilot` controls state/color/brightness (unicast to the bulb's IP).
//! - `getPilot` reads the bulb's current state.
//!
//! All public functions are async and use `tokio::net::UdpSocket`.

use crate::models::Rgb;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::timeout;
use tracing::{debug, info, warn};

/// WiZ bulbs always listen on this UDP port.
const WIZ_PORT: u16 = 38899;

/// Default per-call response timeout for unicast requests.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// A bulb seen on the LAN during a discovery sweep.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredBulb {
    /// Lowercase MAC with no separators (e.g. `"a8bb50a4f94d"`).
    pub mac: String,
    /// IPv4 address the bulb replied from.
    pub ip: Ipv4Addr,
    /// Module name reported by the bulb (e.g. `"ESP15_SHRGB1W_01I"`), if any.
    pub module: Option<String>,
}

/// Snapshot of a bulb's runtime state, as returned by `getPilot`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BulbState {
    /// `true` if the bulb is on.
    pub state: bool,
    pub r: u8,
    pub g: u8,
    pub b: u8,
    /// Brightness percentage, 10..=100.
    pub dimming: u8,
    /// Color temperature in Kelvin (0 if the bulb is in RGB mode).
    pub temp: u32,
    /// WiFi RSSI in dBm (closer to 0 = stronger).
    pub rssi: i32,
    /// Active scene id (0 = no scene).
    pub scene_id: u32,
}

/// Errors that can come out of the bulb module.
#[derive(thiserror::Error, Debug)]
pub enum BulbError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("timeout waiting for bulb response")]
    Timeout,
    #[error("invalid response: {0}")]
    InvalidResponse(String),
}

// ---------- wire format ----------

#[derive(Serialize)]
struct WizRequest<'a, P: Serialize> {
    method: &'a str,
    params: P,
}

#[derive(Serialize)]
struct RegistrationParams<'a> {
    #[serde(rename = "phoneMac")]
    phone_mac: &'a str,
    register: bool,
    #[serde(rename = "phoneIp")]
    phone_ip: &'a str,
}

#[derive(Serialize)]
struct SetPilotOnParams {
    state: bool,
    r: u8,
    g: u8,
    b: u8,
    dimming: u8,
}

#[derive(Serialize)]
struct SetPilotOffParams {
    state: bool,
}

#[derive(Serialize)]
struct EmptyParams {}

#[derive(Deserialize, Debug)]
struct WizResponse {
    #[allow(dead_code)]
    method: Option<String>,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

// ---------- helpers ----------

fn normalize_mac(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Send a single JSON request to a bulb and wait for its reply.
async fn request(ip: Ipv4Addr, payload: Vec<u8>) -> Result<serde_json::Value, BulbError> {
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    let target = SocketAddrV4::new(ip, WIZ_PORT);

    debug!(
        "wiz request -> {}: {}",
        target,
        std::str::from_utf8(&payload).unwrap_or("<binary>")
    );

    sock.send_to(&payload, target).await?;

    let mut buf = vec![0u8; 2048];
    let recv = timeout(RESPONSE_TIMEOUT, sock.recv_from(&mut buf))
        .await
        .map_err(|_| BulbError::Timeout)??;

    let (n, from) = recv;
    debug!(
        "wiz response <- {}: {}",
        from,
        std::str::from_utf8(&buf[..n]).unwrap_or("<binary>")
    );

    let resp: WizResponse = serde_json::from_slice(&buf[..n])?;
    if let Some(err) = resp.error {
        return Err(BulbError::InvalidResponse(format!("bulb error: {}", err)));
    }
    resp.result
        .ok_or_else(|| BulbError::InvalidResponse("missing 'result' field".into()))
}

// ---------- public API ----------

/// Collect IPv4 broadcast addresses of every up, non-loopback interface.
/// Used by [`discover`] to send subnet-directed broadcasts in addition to
/// the limited broadcast `255.255.255.255`. macOS in particular often drops
/// limited broadcasts or only emits them on the default-route interface,
/// so subnet-directed broadcasts are far more reliable.
fn local_broadcast_targets() -> Vec<Ipv4Addr> {
    let mut targets = Vec::new();
    match if_addrs::get_if_addrs() {
        Ok(ifs) => {
            for iface in ifs {
                if iface.is_loopback() {
                    continue;
                }
                if let if_addrs::IfAddr::V4(v4) = iface.addr {
                    if let Some(bcast) = v4.broadcast {
                        if !targets.contains(&bcast) {
                            debug!("local broadcast target {} via {}", bcast, iface.name);
                            targets.push(bcast);
                        }
                    }
                }
            }
        }
        Err(e) => warn!("could not enumerate network interfaces: {}", e),
    }
    targets
}

/// Broadcast UDP discovery on the LAN and collect every bulb that replies
/// during the `wait` window. A reasonable default is 3 seconds.
pub async fn discover(wait: Duration) -> Result<Vec<DiscoveredBulb>, BulbError> {
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    sock.set_broadcast(true)?;

    let payload = serde_json::to_vec(&WizRequest {
        method: "registration",
        params: RegistrationParams {
            phone_mac: "AAAAAAAAAAAA",
            register: false,
            phone_ip: "0.0.0.0",
        },
    })?;

    // Send to limited broadcast AND every local interface's subnet broadcast.
    // Many WiFi APs drop 255.255.255.255 packets; subnet-directed are reliable.
    let mut targets: Vec<SocketAddr> =
        vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), WIZ_PORT)];
    for bcast in local_broadcast_targets() {
        targets.push(SocketAddr::new(IpAddr::V4(bcast), WIZ_PORT));
    }

    info!(
        "broadcasting WiZ discovery to {} target(s) (waiting {:?})",
        targets.len(),
        wait
    );
    for target in &targets {
        if let Err(e) = sock.send_to(&payload, target).await {
            warn!("send_to {} failed: {}", target, e);
        } else {
            debug!("discovery sent to {}", target);
        }
    }

    let mut found: Vec<DiscoveredBulb> = Vec::new();
    let mut buf = vec![0u8; 2048];

    // Loop receiving until the overall `wait` budget runs out.
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match timeout(remaining, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) => {
                let payload = &buf[..n];
                debug!(
                    "discovery reply from {}: {}",
                    from,
                    std::str::from_utf8(payload).unwrap_or("<binary>")
                );

                let ip = match from.ip() {
                    IpAddr::V4(v4) => v4,
                    IpAddr::V6(_) => {
                        warn!("ignoring IPv6 discovery reply from {}", from);
                        continue;
                    }
                };

                match serde_json::from_slice::<WizResponse>(payload) {
                    Ok(resp) => {
                        let Some(result) = resp.result else {
                            warn!("discovery reply from {} had no 'result'", from);
                            continue;
                        };
                        let mac = result
                            .get("mac")
                            .and_then(|v| v.as_str())
                            .map(normalize_mac);
                        let module = result
                            .get("moduleName")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());

                        let Some(mac) = mac else {
                            warn!("discovery reply from {} missing mac field", from);
                            continue;
                        };

                        // De-duplicate by MAC in case a bulb replies twice.
                        if found.iter().any(|b| b.mac == mac) {
                            continue;
                        }

                        info!("discovered bulb {} at {}", mac, ip);
                        found.push(DiscoveredBulb { mac, ip, module });
                    }
                    Err(e) => {
                        warn!("failed to parse discovery reply from {}: {}", from, e);
                    }
                }
            }
            Ok(Err(e)) => {
                warn!("recv_from failed during discovery: {}", e);
                return Err(BulbError::Io(e));
            }
            Err(_) => {
                // Window elapsed.
                break;
            }
        }
    }

    info!("discovery complete: {} bulb(s) found", found.len());
    Ok(found)
}

/// Turn the bulb on with the given RGB color and brightness (10..=100).
pub async fn set_pilot_color(ip: Ipv4Addr, color: Rgb, dimming: u8) -> Result<(), BulbError> {
    // WiZ accepts dimming in 10..=100; clamp so callers don't have to.
    let dimming = dimming.clamp(10, 100);

    let payload = serde_json::to_vec(&WizRequest {
        method: "setPilot",
        params: SetPilotOnParams {
            state: true,
            r: color.r,
            g: color.g,
            b: color.b,
            dimming,
        },
    })?;

    let result = request(ip, payload).await?;
    // setPilot returns {"success": true}; treat anything else as a soft failure.
    if result.get("success").and_then(|v| v.as_bool()) == Some(false) {
        return Err(BulbError::InvalidResponse(format!(
            "setPilot success=false: {}",
            result
        )));
    }
    debug!(
        "set_pilot_color {} -> rgb({},{},{}) dim={}",
        ip, color.r, color.g, color.b, dimming
    );
    Ok(())
}

/// Turn the bulb off.
pub async fn set_pilot_off(ip: Ipv4Addr) -> Result<(), BulbError> {
    let payload = serde_json::to_vec(&WizRequest {
        method: "setPilot",
        params: SetPilotOffParams { state: false },
    })?;

    let result = request(ip, payload).await?;
    if result.get("success").and_then(|v| v.as_bool()) == Some(false) {
        return Err(BulbError::InvalidResponse(format!(
            "setPilot success=false: {}",
            result
        )));
    }
    debug!("set_pilot_off {}", ip);
    Ok(())
}

/// Query a bulb's current state. Times out after 2 seconds.
pub async fn get_pilot(ip: Ipv4Addr) -> Result<BulbState, BulbError> {
    let payload = serde_json::to_vec(&WizRequest {
        method: "getPilot",
        params: EmptyParams {},
    })?;

    let result = request(ip, payload).await?;

    let get_u64 = |k: &str| -> u64 { result.get(k).and_then(|v| v.as_u64()).unwrap_or(0) };
    let get_i64 = |k: &str| -> i64 { result.get(k).and_then(|v| v.as_i64()).unwrap_or(0) };
    let state = result
        .get("state")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| BulbError::InvalidResponse("missing 'state' field".into()))?;

    let bulb_state = BulbState {
        state,
        r: get_u64("r").min(255) as u8,
        g: get_u64("g").min(255) as u8,
        b: get_u64("b").min(255) as u8,
        dimming: get_u64("dimming").min(100) as u8,
        temp: get_u64("temp") as u32,
        rssi: get_i64("rssi") as i32,
        scene_id: get_u64("sceneId") as u32,
    };

    debug!("get_pilot {} -> {:?}", ip, bulb_state);
    Ok(bulb_state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_mac_strips_separators_and_lowercases() {
        assert_eq!(normalize_mac("A8:BB:50:A4:F9:4D"), "a8bb50a4f94d");
        assert_eq!(normalize_mac("a8bb50a4f94d"), "a8bb50a4f94d");
        assert_eq!(normalize_mac("A8-BB-50-A4-F9-4D"), "a8bb50a4f94d");
    }
}
