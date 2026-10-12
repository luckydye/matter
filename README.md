# matter-server

A standalone Matter controller built on [rs-matter](https://github.com/project-chip/rs-matter). It discovers Matter nodes over mDNS and commissions them onto its own fabric. Devices that aren't on the network yet are commissioned over Bluetooth LE and put on your Wi-Fi. You drive it through an HTTP JSON API.

```sh
cargo run -- [--listen 127.0.0.1:5580] [--interface en0] [--storage ./matter-storage]
```

| Request | Description |
| --- | --- |
| `GET /devices` | Discovered devices, one entry each with its `commissionable` (`_matterc._udp`) and `operational` (`_matter._tcp`) mDNS records and its `bluetooth` advertisement |
| `GET /nodes` | Nodes commissioned onto our fabric, with their operational mDNS record if they're online |
| `GET /nodes/<id>` | The node's light: `{"endpoint": 1, "on": true, "level": 254}` (`level` is brightness 1-254, `null` if not dimmable) |
| `POST /nodes/<id>` `{"on": true, "level": 127}` | Switch the light and/or set its brightness (either field optional; setting `level` also turns it on) |
| `POST /nodes/<id>/toggle` | Toggle the light |
| `POST /commission` `{"code": "3497-0112-332"}` | Commission the discovered device matching a manual pairing code or an `MT:` QR payload |
| `POST /commission` `{"code": "...", "wifi": {"ssid": "...", "password": "..."}}` | Same, but if no device on the network matches, scan Bluetooth for it and put it on this Wi-Fi network |

```sh
curl localhost:5580/devices
curl -X POST localhost:5580/nodes/1 -d '{"level": 127}'
curl -X POST localhost:5580/nodes/1/toggle
curl -X POST localhost:5580/commission -d '{"code": "34970112332"}'
curl -X POST localhost:5580/commission -d '{"code": "34970112332", "wifi": {"ssid": "home", "password": "secret"}}'
```

The fabric, its ICAC signing key and the list of commissioned nodes are stored in `--storage`.

## Limitations

- Discovery is IPv6 mDNS on a single interface. Without `--interface`, the first interface with a link-local IPv6 address is used.
- Device attestation isn't verified, because rs-matter doesn't support DCL verification yet. Every device is accepted.
- Bluetooth commissioning provisions Wi-Fi only, not Thread. Bluetooth-only devices are listed in `GET /devices`, but the pairing code is still needed to commission them: its passcode is never advertised.
- On macOS the terminal app running the server needs Bluetooth access (System Settings → Privacy & Security → Bluetooth).

## Test

`tests/commission.sh` builds rs-matter's `onoff_light` example, then discovers it, commissions it, and checks that the node survives a server restart.
