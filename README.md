# Lutron Caséta

Caséta dimmers over LEAP, the TLS protocol every current Smart Bridge speaks.

| Package | Drivers | Needs |
| --- | --- | --- |
| [`leap/`](leap) | `lutron.caseta.leap_bridge`, `lutron.caseta.leap_dimmer`, `lutron.caseta.leap_pico` | Any current Smart Bridge |

## Pairing (LEAP)

LEAP needs a client certificate the bridge has signed. The driver generates a keypair and a
CSR, submits it on the pairing port while the bridge's button is held, and stores what comes
back per device.

That bootstrap exchange uses the client identity published as part of
[`pylutron_caseta`](https://github.com/gurumitts/pylutron-caseta) — the same one every
third-party integration presents. It is **not a secret**: pairing is a chicken-and-egg problem,
and a client needs some certificate the bridge already trusts before it can be issued a real
one. It grants nothing beyond "let me submit a CSR". The per-installation certificate the
bridge signs in response is what actually authorizes anything afterward.

## Checking fixtures against real hardware

```bash
cargo run --release --example leap_probe -- <bridge-ip>
```

Drives the controller's own TLS and LEAP transport rather than a reimplementation, so what it
proves is what the driver will actually see.

## Building

```bash
cargo build --release -p juno-driver-lutron-caseta-leap
```

Releases are built by [`junohouse/driver-ci`](https://github.com/junohouse/driver-ci): push to
`main` for a beta, tag `v1.2.0` for a release.
