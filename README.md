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

## The connection is never idle

The bridge hangs up a client that says nothing, within minutes. Everything this driver exists
for — a press, a wall dimmer moving — arrives unasked on that connection, so a hang-up costs
every event until the controller notices and dials again.

Both children therefore declare a heartbeat: `/server/1/status/ping` every sixty seconds, which
the bridge answers with `OnePingResponse` and nothing else. Core owns the timer, because core
owns the socket and is the only thing that knows when it last carried anything. See
`ControlDecl::heartbeat`.

## What a Pico's keys are called

The bridge will not say. It lists five buttons under a remote that has two, and names every one
of them `Button N` whatever is in somebody's hand — so the model is the only thing that says
which `ButtonNumber`s are real and what they are engraved with. `PICO_KEYS` is that table,
the same one `pylutron-caseta` and Home Assistant keep. A model that is not in it keeps every
key the bridge listed, numbered: `Button 3` that presses beats `Off` that does not.

Which is also why the second read is `/button` and not `/buttongroup`. A group lists hrefs in an
order that is not `ButtonNumber`, and on a remote with fewer keys than the group has entries,
that order names every one of them wrong.

Some hardware misreports itself and nothing can be done about it: an MRF2-3B-L arrives as a
`Pico3ButtonRaiseLower` with `ModelNumber: PJ-3BRL-GXX-XXX`. Its raise and lower keys are drawn
and never fire, because on the wire it is indistinguishable from a remote that has them.

## Battery

`/device/<n>/status` carries `BatteryStatus`, so a remote whose cell has gone stops looking
identical to one nobody has pressed. Lutron reports a word and the keypad contract wants a
percentage, so the mapping is stated in `battery_percent` rather than smuggled: `Good` is full,
`Low` is nearly empty, anything unrecognised reports nothing at all.

A Pico adopted before this existed has `has_battery: false` in its stored contract and core
rightly refuses the notification — nothing rewrites a saved house. Adopt it again through the
bridge and it reconciles in place, keeping its id and its rules.

## What this does not drive

Dimmers and Picos. Switches, fan controllers, occupancy sensors, Serena shades and the
`/virtualbutton` scenes a bridge publishes are all real LEAP things and none of them are here.
Each needs a `DeviceType` arm and a manifest, and — more to the point — hardware to check
against: a switched zone reports `SwitchedLevel` where a dimmer reports `Level`, and guessing
which commands a load accepts is how a driver ends up shipping something nobody has pressed.

## Checking fixtures against real hardware

```bash
cargo run --release --example leap_probe -- <bridge-ip>
```

Drives the controller's own TLS and LEAP transport rather than a reimplementation, so what it
proves is what the driver will actually see.

For anything about *which* connection carried what — subscriptions arriving on separate sockets,
a heartbeat that never went out — put a TLS-terminating proxy between the controller and the
bridge and read the LEAP traffic directly. The controller accepts any certificate for a bare IP
(see `transport::tls`), so the bridge's own client identity does duty as the proxy's server
identity, and `Address` on the bridge device points at the proxy.

## Building

```bash
cargo build --release -p juno-driver-lutron-caseta-leap
```

Releases are built by [`junohouse/driver-ci`](https://github.com/junohouse/driver-ci): push to
`main` for a beta, tag `v1.2.0` for a release.
