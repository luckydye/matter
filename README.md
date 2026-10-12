# matter-server

A standalone Matter controller with an HTTP JSON API, built on [rs-matter](https://github.com/project-chip/rs-matter). It discovers devices over mDNS and Bluetooth, commissions them onto its own fabric (putting Bluetooth devices on your Wi-Fi), and controls lights.

```sh
cargo run -- [--listen 127.0.0.1:5580] [--interface en0] [--storage ./matter-storage]
```

## HTTP API

| Route | Body | Description |
| --- | --- | --- |
| `GET /devices` | | Discovered devices, one entry each with its `commissionable` / `operational` mDNS records and `bluetooth` advertisement |
| `POST /commission` | `{"code": "...", "wifi": {"ssid": "...", "password": "..."}}` | Commission a device by manual pairing code or `MT:` QR payload. With `wifi`, a device not on the network is found over Bluetooth and joined to that network |
| `GET /nodes` | | Commissioned nodes, with their operational mDNS record when online |
| `GET /nodes/<id>` | | Light state: `{"node_id": 1, "endpoint": 1, "on": true, "level": 254, "color": true}` |
| `POST /nodes/<id>` | `{"on": true, "level": 127, "color": "#ff0000"}` | Change the light; all fields optional. `level` is brightness 1-254 (also turns it on), `color` sets hue and saturation |
| `POST /nodes/<id>/toggle` | | Toggle the light |

Errors are `{"error": "..."}` with status 400 (bad request), 404 (unknown device or node), 405 (wrong method), 504 (device did not respond) or 500.

## Usage

Put the bulb in pairing mode, then commission it with the code from its label:

```sh
curl localhost:5580/devices
curl -X POST localhost:5580/commission -d '{"code": "2424-211-0896", "wifi": {"ssid": "home", "password": "secret"}}'
# {"node_id": 1, "operational": {...}}
```

Control it:

```sh
curl localhost:5580/nodes/1
curl -X POST localhost:5580/nodes/1 -d '{"on": false}'
curl -X POST localhost:5580/nodes/1 -d '{"level": 64, "color": "#ff8000"}'
curl -X POST localhost:5580/nodes/1/toggle
```

## Notes

- `--storage` holds the fabric, its signing key and the commissioned nodes. Keep it private.
- Bluetooth on macOS needs Apple's [Bluetooth Central Matter Client Developer Mode](https://developer.apple.com/services-account/download?path=/iOS/iOS_Logs/EnableBluetoothCentralMatterClientDeveloperMode.mobileconfig) profile, and Bluetooth access for the terminal app (System Settings → Privacy & Security → Bluetooth).
- Bluetooth commissioning provisions Wi-Fi only, not Thread.
- Device attestation isn't verified (rs-matter has no DCL support yet); every device is accepted.
- Discovery runs on one network interface; without `--interface`, the first with link-local IPv6 and IPv4 is used.

## Test

`tests/commission.sh` builds rs-matter's `onoff_light` example, then discovers it, commissions it, and checks that the node survives a server restart.
