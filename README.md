# matter-server

A standalone Matter controller with an HTTP JSON API, built on [rs-matter](https://github.com/project-chip/rs-matter). It discovers devices over mDNS and Bluetooth, commissions them onto its own fabric (putting Bluetooth devices on your Wi-Fi), and controls lights.

```sh
cargo run -- [--listen 127.0.0.1:5580] [--interface en0] [--storage ./matter-storage]
```

## HTTP API

| Route | Body | Description |
| --- | --- | --- |
| `GET /` | | A page to control the lights (power, brightness, color, all at once) |
| `GET /devices` | | Discovered devices, one entry each with its `commissionable` / `operational` mDNS records and `bluetooth` advertisement |
| `POST /commission` | `{"code": "...", "wifi": {"ssid": "...", "password": "..."}}` | Commission a device by manual pairing code or `MT:` QR payload. With `wifi`, a device not on the network is found over Bluetooth and joined to that network |
| `GET /nodes` | | Commissioned nodes, with their operational mDNS record when online |
| `GET /nodes/<id>` | | Light state: `{"node_id": 1, "endpoint": 1, "on": true, "level": 254, "color": "#ff0000"}` (`level` / `color` are `null` if not supported; `color` is at full brightness) |
| `POST /nodes/<id>` | `{"on": true, "level": 127, "color": "#ff0000", "transition": 0.5}` | Change the light; all fields optional. `level` is brightness 1-254 (also turns it on), `color` is sent as CIE xy (hue/saturation for lights without xy support) so the light reproduces it with its own calibration, `transition` fades `level` and `color` over that many seconds. Answers `204` |
| `POST /nodes/<id>/toggle` | | Toggle the light. Answers `204` |

Errors are `{"error": "..."}` with status 400 (bad request), 404 (unknown device or node), 405 (wrong method), 504 (device did not respond) or 500.

## Usage

Put the bulb in pairing mode, then commission it with the code from its label:

```sh
curl localhost:5580/devices
curl -X POST localhost:5580/commission -d '{"code": "2424-211-0896", "wifi": {"ssid": "home", "password": "secret"}}'
# {"node_id": 1, "operational": {...}}
```

Control it from http://localhost:5580, or with curl:

```sh
curl localhost:5580/nodes/1
curl -X POST localhost:5580/nodes/1 -d '{"on": false}'
curl -X POST localhost:5580/nodes/1 -d '{"level": 64, "color": "#ff8000"}'
curl -X POST localhost:5580/nodes/1/toggle
```

For animations, send keyframes with a `transition` about as long as the interval between them; the bulb fades in between. Each command is one round trip: ~30 ms per field, ~100 ms for all four bulbs in parallel, with Wi-Fi power-save spikes of a few hundred ms:

```sh
while true; do
  for color in "#ff0000" "#00ff00" "#0000ff"; do
    curl -s -X POST localhost:5580/nodes/1 -d "{\"color\": \"$color\", \"transition\": 1}"
    sleep 1
  done
done
```

## Notes

- `--storage` holds the fabric, its signing key and the commissioned nodes. Keep it private.
- Bluetooth on macOS needs Apple's [Bluetooth Central Matter Client Developer Mode](https://developer.apple.com/services-account/download?path=/iOS/iOS_Logs/EnableBluetoothCentralMatterClientDeveloperMode.mobileconfig) profile, and Bluetooth access for the terminal app (System Settings → Privacy & Security → Bluetooth).
- Bluetooth commissioning provisions Wi-Fi only, not Thread.
- Device attestation isn't verified (rs-matter has no DCL support yet); every device is accepted.
- Discovery runs on one network interface; without `--interface`, the first with link-local IPv6 and IPv4 is used.

## Test

`tests/commission.sh` builds rs-matter's `onoff_light` example, then discovers it, commissions it, and checks that the node survives a server restart.
