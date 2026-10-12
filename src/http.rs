//! A minimal HTTP/1.1 JSON API.
//!
//! - `GET  /devices`   - all Matter devices currently discovered, one entry each with its mDNS records and Bluetooth advertisement
//! - `GET  /nodes`     - nodes commissioned onto our fabric
//! - `GET  /nodes/<id>` - the node's light: `{"on": bool, "level": 1-254, "color": bool}`
//! - `POST /nodes/<id>` with `{"on": bool, "level": 1-254, "color": "#rrggbb"}` (all fields optional)
//! - `POST /nodes/<id>/toggle`
//! - `POST /commission` with `{"code": "<manual pairing code or MT:... QR payload>"}`,
//!   plus `"wifi": {"ssid": "...", "password": "..."}` to also search Bluetooth

use std::net::{SocketAddr, TcpListener, TcpStream};

use async_executor::LocalExecutor;
use async_io::Async;
use futures_lite::{AsyncReadExt, AsyncWriteExt};
use log::{info, warn};
use serde::Deserialize;
use serde_json::{json, Value};

use rs_matter::crypto::Crypto;
use rs_matter::error::{Error, ErrorCode};
use rs_matter::persist::KvBlobStoreAccess;

use crate::ble::BleScanner;
use crate::control::{hue_saturation, LightChange, LightState};
use crate::controller::{parse_code, Controller, WiFi};
use crate::devices::devices;
use crate::discovery::Discovery;

const MAX_REQUEST_LEN: usize = 64 * 1024;

pub struct Response {
    pub status: u16,
    pub body: Value,
}

impl Response {
    fn ok(body: Value) -> Self {
        Self { status: 200, body }
    }

    fn error(status: u16, message: impl ToString) -> Self {
        Self {
            status,
            body: json!({ "error": message.to_string() }),
        }
    }
}

#[derive(Deserialize)]
struct CommissionRequest {
    code: String,
    wifi: Option<WiFi>,
}

pub async fn serve<'a, C: Crypto, K: KvBlobStoreAccess>(
    addr: SocketAddr,
    executor: &LocalExecutor<'a>,
    discovery: &'a Discovery,
    ble: &'a BleScanner,
    controller: &'a Controller<'a, C, K>,
) -> Result<(), Error> {
    let listener = Async::<TcpListener>::bind(addr)?;
    info!("HTTP API listening on http://{addr}");

    loop {
        let (stream, peer) = listener.accept().await?;

        // Each connection runs as its own task so a long commissioning does not block `GET /devices`.
        executor
            .spawn(async move {
                if let Err(e) = handle_connection(stream, discovery, ble, controller).await {
                    warn!("HTTP connection from {peer} failed: {e:?}");
                }
            })
            .detach();
    }
}

async fn handle_connection<C: Crypto, K: KvBlobStoreAccess>(
    mut stream: Async<TcpStream>,
    discovery: &Discovery,
    ble: &BleScanner,
    controller: &Controller<'_, C, K>,
) -> Result<(), Error> {
    let response = match read_request(&mut stream).await? {
        Some((method, path, body)) => {
            route(&method, &path, &body, discovery, ble, controller).await
        }
        None => Response::error(400, "malformed HTTP request"),
    };

    let body = response.body.to_string();
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        reason(response.status),
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;

    Ok(())
}

/// Read one request, returning `(method, path, body)`, or `None` if it is malformed.
async fn read_request(
    stream: &mut Async<TcpStream>,
) -> Result<Option<(String, String, Vec<u8>)>, Error> {
    let mut data = Vec::new();
    let mut chunk = [0u8; 4096];

    let header_end = loop {
        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if data.len() > MAX_REQUEST_LEN {
            return Ok(None);
        }
        let len = stream.read(&mut chunk).await?;
        if len == 0 {
            return Ok(None);
        }
        data.extend_from_slice(&chunk[..len]);
    };

    let Ok(head) = std::str::from_utf8(&data[..header_end]) else {
        return Ok(None);
    };
    let mut lines = head.lines();
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let (Some(method), Some(path)) = (request_line.next(), request_line.next()) else {
        return Ok(None);
    };
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>())
        .transpose();
    let Ok(content_length) = content_length else {
        return Ok(None);
    };
    let content_length = content_length.unwrap_or(0);
    if content_length > MAX_REQUEST_LEN {
        return Ok(None);
    }

    let (method, path) = (method.to_string(), path.to_string());
    let mut body = data.split_off(header_end);
    while body.len() < content_length {
        let len = stream.read(&mut chunk).await?;
        if len == 0 {
            return Ok(None);
        }
        body.extend_from_slice(&chunk[..len]);
    }
    body.truncate(content_length);

    Ok(Some((method, path, body)))
}

async fn route<C: Crypto, K: KvBlobStoreAccess>(
    method: &str,
    path: &str,
    body: &[u8],
    discovery: &Discovery,
    ble: &BleScanner,
    controller: &Controller<'_, C, K>,
) -> Response {
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    // A node path segment is only valid for a node commissioned onto our fabric.
    let node_id = |id: &str| {
        id.parse::<u64>()
            .ok()
            .filter(|id| controller.nodes.borrow().contains(id))
    };

    match (method, segments.as_slice()) {
        ("GET", ["devices"]) => Response::ok(json!(devices(discovery, ble))),
        ("GET", ["nodes"]) => Response::ok(json!(controller.nodes())),
        ("GET", ["nodes", id]) => match node_id(id) {
            Some(id) => light_response(controller.light_state(id).await),
            None => Response::error(404, "no such node"),
        },
        ("POST", ["nodes", id]) => {
            let Some(id) = node_id(id) else {
                return Response::error(404, "no such node");
            };
            let change: LightChange = match serde_json::from_slice(body) {
                Ok(change) => change,
                Err(e) => return Response::error(400, e),
            };
            if change
                .level
                .is_some_and(|level| !(1..=254).contains(&level))
            {
                return Response::error(400, "level must be 1-254");
            }
            if change
                .color
                .as_deref()
                .is_some_and(|color| hue_saturation(color).is_none())
            {
                return Response::error(400, "color must be #rrggbb");
            }
            light_response(controller.change_light(id, &change).await)
        }
        ("POST", ["nodes", id, "toggle"]) => match node_id(id) {
            Some(id) => light_response(controller.toggle_light(id).await),
            None => Response::error(404, "no such node"),
        },
        ("POST", ["commission"]) => {
            let request: CommissionRequest = match serde_json::from_slice(body) {
                Ok(request) => request,
                Err(e) => return Response::error(400, e),
            };

            let Ok((filter, passcode)) = parse_code(&request.code) else {
                return Response::error(400, "invalid pairing code");
            };

            match controller
                .commission(&filter, passcode, request.wifi.as_ref())
                .await
            {
                Ok(Some(node)) => Response::ok(json!(node)),
                Ok(None) => Response::error(404, "no commissionable device matches this code"),
                Err(e) if e.code() == ErrorCode::RxTimeout => {
                    Response::error(504, "commissioning timed out")
                }
                Err(e) => Response::error(500, format!("commissioning failed: {:?}", e.code())),
            }
        }
        (_, ["devices" | "nodes" | "commission"] | ["nodes", _] | ["nodes", _, "toggle"]) => {
            Response::error(405, "method not allowed")
        }
        _ => Response::error(404, "not found"),
    }
}

fn light_response(result: Result<LightState, Error>) -> Response {
    match result {
        Ok(state) => Response::ok(json!(state)),
        Err(e) if e.is_peer_unresponsive() => Response::error(504, "node did not respond"),
        Err(e) if e.code() == ErrorCode::NotFound => Response::error(
            404,
            "node not found on the network, or it has no light (OnOff) endpoint",
        ),
        Err(e) if e.code() == ErrorCode::InvalidCommand => {
            Response::error(400, "light does not support this change (dimming or color)")
        }
        Err(e) => Response::error(500, format!("light control failed: {:?}", e.code())),
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        504 => "Gateway Timeout",
        _ => "Internal Server Error",
    }
}
