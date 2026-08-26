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

## What a zone turns out to be

Dimmers, switches, fan controllers and shades are all `/zone`s, and on the wire they are
indistinguishable — same status shape, same command processor. What tells them apart is
`DeviceType`, so the four lists (`DIMMABLE`, `SWITCHED`, `FANS`, `COVERS`) are transcribed from
`pylutron-caseta`'s own `_LEAP_DEVICE_TYPES` rather than guessed. A type missing from them is a
device that silently cannot be added, which is why they are long.

The answer is written down as a `Kind` property at adoption and read back on every command and
every status, because `Instance` carries properties and nothing else — a driver has no other way
to know what it is. Anything adopted before that existed has no `Kind` and reads as a light,
which is what it was.

What each one sends is not interchangeable, and that is the reason for the split rather than a
`dimmer = false` capability:

| | command | status field |
| --- | --- | --- |
| dimmer | `GoToDimmedLevel` with a fade | `Level` |
| switch | `GoToLevel` — no fade to give it | `Level`, as 0 or 100 |
| fan | `GoToFanSpeed` | `FanSpeed`, a word |
| shade | `GoToLevel`, `Raise`/`Lower`/`Stop`, `GoToTilt` | `Level` and `Tilt` |

Fan speeds are the house's words on this side and Lutron's on the wire — a rule here says
`medium_high` and only `FAN_SPEEDS` knows that LEAP calls it `MediumHigh`. Shade tilt is a
capability the model decides at adoption: a roller shade offering a tilt control is a control
that does nothing.

## What this still does not drive

Occupancy sensors. They are the one LEAP group that is not a zone — they arrive through
`/occupancygroup`, a separate collection with its own subscription — so they want their own read
rather than another `DeviceType` arm.

The `/virtualbutton` scenes a bridge publishes are also absent. Those are a Caséta scene, which
is a question about how a provider's scenes meet the house's own, not about this protocol.

None of the four above has been driven against real hardware — there was none to hand. The wire
formats come from `pylutron-caseta`, the tests pin the translations in both directions, and the
Pico path is verified end to end; the rest is faithful transcription that nobody has pressed.

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
