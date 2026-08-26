//! Lutron Caséta over LEAP — the bridge's TLS API port, as opposed to the PRO-only plaintext
//! integration port `lutron.caseta.dimmer` speaks.
//!
//! Three manifests share this one module: `lutron.caseta.leap_bridge` (the parent — pairing
//! lives here), `lutron.caseta.leap_dimmer` (a zone behind it) and `lutron.caseta.leap_pico`
//! (a battery remote behind it). Neither child is ever set up on its own; both are found by
//! browsing an already-paired bridge, the same way a Hue bulb is found by browsing a Hue
//! bridge.
//!
//! LEAP itself is not HTTP. It is one JSON object per line over a TLS socket:
//!
//! - Port 8083, pairing: connect presenting Lutron's own published pairing identity (see
//!   [`lap_identity`] — every third-party LEAP client uses this same one; it exists so a
//!   client can talk to the bridge *before* it has an identity of its own), wait for the
//!   bridge to push confirmation that its button was physically pressed, then submit a CSR
//!   and get back a certificate signed for this installation specifically.
//! - Port 8081, mutual TLS with that certificate: everything else. A `ReadRequest` for
//!   `/device` lists what is paired, `/button` lists every key on every remote; a
//!   `SubscribeRequest` on a zone's `/status` or a button's `/status/event` is how a press —
//!   on the wall or on a remote — reaches us without polling.
//!
//! **The controller has no idea any of this is going on, and that is the point.** LEAP used
//! to live in core: the ports, the line framing, skipping the acknowledgements a bridge
//! volunteers, and waiting for the button push were all its business, reached through a
//! `leaps://` pseudo-URL. One vendor's protocol in the controller everyone runs.
//!
//! Now the controller offers only what any device might need — open a connection, optionally
//! with a client certificate, write bytes, listen for a while — as `SetupStep::Session`. It
//! holds the socket, so this driver still never touches one and stays sandboxable. Everything
//! above the socket is here, where the knowledge is:
//!
//! - which ports ([`PAIR_PORT`], [`LEAP_PORT`])
//! - where one message ends ([`leap_line`], [`leap_objects`])
//! - which reply is the answer rather than an acknowledgement ([`leap_answer`])
//! - what a button press looks like ([`CasetaLeap::button_pressed`])
//!
//! The live connection rides the same `control: 0` Tx/rx path the integration-port driver
//! uses, just wrapped in TLS because the manifest says so.

mod lap_identity;

use driver_sdk::*;
use std::collections::BTreeMap;

const BRIDGE_ID: &str = "lutron.caseta.leap_bridge";
const DIMMER_ID: &str = "lutron.caseta.leap_dimmer";
const PICO_ID: &str = "lutron.caseta.leap_pico";
const SWITCH_ID: &str = "lutron.caseta.leap_switch";
const FAN_ID: &str = "lutron.caseta.leap_fan";
const SHADE_ID: &str = "lutron.caseta.leap_shade";
const OCCUPANCY_ID: &str = "lutron.caseta.leap_occupancy";

/// What a zone behind this bridge turns out to be.
///
/// Every one of them is a `/zone` with a level, and that is exactly why this exists: the wire
/// looks the same for a dimmer, a switch, a fan and a shade, while the proxy each one presents
/// and the notification each one owes are different. Written at adoption under `Kind` and read
/// back on every command and every status — the same trick the Pico's button hrefs use, and for
/// the same reason: `Instance` carries properties and nothing else, so anything the driver needs
/// to know about itself has to be one.
///
/// A device adopted before this existed has no `Kind` and reads as `Light`, which is what it
/// was — nothing rewrites a saved house.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Light,
    Switch,
    Fan,
    Shade,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Light => "light",
            Kind::Switch => "switch",
            Kind::Fan => "fan",
            Kind::Shade => "shade",
        }
    }

    fn of(inst: &Instance) -> Kind {
        match inst.property("Kind").as_str().unwrap_or("") {
            "switch" => Kind::Switch,
            "fan" => Kind::Fan,
            "shade" => Kind::Shade,
            _ => Kind::Light,
        }
    }
}

/// Which LEAP `DeviceType`s are which, transcribed from `pylutron-caseta`'s own
/// `_LEAP_DEVICE_TYPES`. Not guessed and not shortened: the list is long because Lutron has
/// shipped a lot of hardware, and a type missing from it is a device that silently cannot be
/// added rather than one that behaves oddly.
///
/// `sensor` is deliberately absent. Everything in that group is a keypad or a remote, which
/// this driver already reaches through `is_pico` and the button collection — they are not
/// zones and have no level to read.
const DIMMABLE: &[&str] = &[
    "WallDimmer", "PlugInDimmer", "InLineDimmer", "SunnataDimmer", "TempInWallPaddleDimmer",
    "WallDimmerWithPreset", "Dimmed", "SpectrumTune", "DivaSmartDimmer", "WhiteTune",
    "PowPak0-10V", "ColorTune",
];
const SWITCHED: &[&str] = &[
    "WallSwitch", "OutdoorPlugInSwitch", "PlugInSwitch", "InLineSwitch", "PowPakSwitch",
    "SunnataSwitch", "TempInWallPaddleSwitch", "Switched", "KeypadLED", "DivaSmartSwitch",
];
const FANS: &[&str] = &["CasetaFanSpeedController", "MaestroFanSpeedController", "FanSpeed"];
const COVERS: &[&str] = &[
    "SerenaHoneycombShade", "SerenaRollerShade", "TriathlonHoneycombShade",
    "TriathlonEssentialsRollerShade", "TriathlonRollerShade", "TriathlonTiltOnlyWoodBlind",
    "QsWirelessShade", "QsWirelessHorizontalSheerBlind", "QsWirelessWoodBlind", "RightDrawDrape",
    "Shade", "Tilt", "SerenaTiltOnlyWoodBlind", "PalladiomWireFreeShade",
    "SerenaEssentialsRollerShade", "OpenCloseStop",
];

/// The speeds a Caséta fan controller has, in the house's words and on the wire.
///
/// Slowest first, matching the `speeds` capability the manifest declares. The translation lives
/// here because a vendor's spelling is the driver's business: a rule written in this house says
/// `medium_high`, and only this line knows LEAP calls it `MediumHigh`.
const FAN_SPEEDS: &[(&str, &str)] = &[
    ("low", "Low"),
    ("medium", "Medium"),
    ("medium_high", "MediumHigh"),
    ("high", "High"),
];

/// `control: 0` is the driver's own network transport — core owns the socket.
const NET: LocalId = 0;

/// The bridge's unauthenticated pairing port, and its authenticated one. Lutron's numbers,
/// which is why they live here and not in the controller.
const PAIR_PORT: u16 = 8083;
/// The setup flow opens its own socket, so it needs the port here. The *held* connection is
/// core's — it dials `[[transport]] port` from the two device manifests, which is the same 8081
/// written down a second time and cannot be deduplicated from this side. Change one, change all.
const LEAP_PORT: u16 = 8081;

/// The most buttons any Pico has (a Pico3ButtonRaiseLower). An upper bound for reading the
/// hrefs off an instance, not a claim about any one remote — what a given Pico actually has is
/// `pico_keys_of`, answered per device at adoption.
const MAX_BUTTONS: usize = 5;

/// What each key on a Pico is, by model, keyed on `ButtonNumber`.
///
/// The bridge does not say, and cannot be made to. Every key it lists is called `Button 1`
/// through `Button 5` whatever is in somebody's hand — confirmed on a PJ-3BRL, whose five
/// buttons come back with exactly those names and no engraving — and it lists numbers that do
/// not exist on the remote: a `Pico2Button` has two keys and is still filed under a group of
/// five. So which numbers are real, and what they are called, is a property of the model and
/// nothing else. This is the table `pylutron-caseta` and Home Assistant's `lutron_caseta`
/// integration both keep, and this one is transcribed from the latter's
/// `DEVICE_TYPE_SUBTYPE_MAP_TO_LEAP`, which is the same numbers.
///
/// `FourGroupRemote` is deliberately absent: it has twenty-five buttons, five times what a
/// keypad proxy here can carry, so it falls through to the numbered case and gets the first
/// five rather than a name for the wrong key.
///
/// Order is `ButtonNumber` order, not the order they sit on the remote — a 3BRL reads On,
/// Favorite, Off, Raise, Lower, while your thumb finds On, Raise, Favorite, Lower, Off. The
/// keys are named now, so the list is a stable identity rather than a picture of the remote.
const PICO_KEYS: &[(&str, &[(u64, &str)])] = &[
    ("Pico2Button", &[(0, "On"), (2, "Off")]),
    ("PaddleSwitchPico", &[(0, "On"), (2, "Off")]),
    ("Pico2ButtonRaiseLower", &[(0, "On"), (2, "Off"), (3, "Raise"), (4, "Lower")]),
    ("Pico3Button", &[(0, "On"), (1, "Favorite"), (2, "Off")]),
    (
        "Pico3ButtonRaiseLower",
        &[(0, "On"), (1, "Favorite"), (2, "Off"), (3, "Raise"), (4, "Lower")],
    ),
    (
        "Pico4Button",
        &[(1, "Button 1"), (2, "Button 2"), (3, "Button 3"), (4, "Button 4")],
    ),
    (
        "Pico4ButtonScene",
        &[(1, "Button 1"), (2, "Button 2"), (3, "Button 3"), (4, "Off")],
    ),
    ("Pico4ButtonZone", &[(1, "On"), (2, "Raise"), (3, "Lower"), (4, "Off")]),
    (
        "Pico4Button2Group",
        &[
            (1, "Group 1 Button 1"),
            (2, "Group 1 Button 2"),
            (3, "Group 2 Button 1"),
            (4, "Group 2 Button 2"),
        ],
    ),
];

/// How long a key must stay down before it counts as held rather than pressed.
///
/// Half a second, which is the line every keypad in the world draws in roughly the same place:
/// long enough that an ordinary press never crosses it, short enough that somebody holding a
/// button to dim does not wonder whether it is working.
///
/// Lutron does not draw it for us — a Caséta bridge reports `Press` and `Release` and leaves
/// the decision to whoever is listening. Until `HostCall::After` there was nothing here that
/// could decide: a driver has no clock, so `has_hold` was false and hold-to-dim could not be
/// offered on a Pico at all.
const HOLD_MS: u32 = 500;

/// How long a held keypad button takes to run a lamp from one end of its travel to the other.
///
/// Four seconds, which is what it takes to land on a level somebody wanted rather than to
/// arrive at full before their thumb has moved. Long enough to aim, short enough not to feel
/// like waiting.
const RAMP_SECS: u64 = 4;

/// How many one-second polls to give someone to walk over and press the button.
const PAIR_WAIT_SECS: u32 = 30;

/// How many more reads to give the bridge for an answer that has not arrived yet.
///
/// A read returns as soon as the socket goes quiet for a moment, and a Caséta bridge
/// volunteers a `SubscribeResponse` or two the instant a connection opens — so the first read
/// after a `ReadRequest` is routinely all acknowledgement and no answer, with the answer
/// arriving a beat later. `leap_answer` skips acknowledgements within one read; this is the
/// same thing across reads. Without it a bridge with three devices behind it listed none, and
/// the flow said there was nothing to add.
const READ_RETRIES: u32 = 5;

// ---------------------------------------------------------------------------------------
// LEAP framing
//
// One JSON object per line over TLS. The controller hands back whatever bytes arrived in a
// window and does not know where one message ends — deliberately, because this is the part
// that is Lutron's and not every device's.
// ---------------------------------------------------------------------------------------

/// A message, framed as the bridge expects to read it.
fn leap_line(msg: &Value) -> String {
    format!("{msg}\n")
}

/// Every complete JSON object in what arrived, ignoring partial or junk lines.
fn leap_objects(received: &str) -> impl Iterator<Item = Value> + '_ {
    received
        .lines()
        .filter_map(|l| driver_sdk::serde_json::from_str::<Value>(l.trim()).ok())
}

/// The reply to what was asked, as opposed to what the bridge volunteered.
///
/// A bridge answers `/device` with an unsolicited `SubscribeResponse` or two ahead of the
/// actual `ReadResponse` — confirmed against real hardware, and the same thing
/// `pylutron-caseta`'s own read loop does: keep reading past acknowledgements it did not ask
/// for. Taking the first object instead would silently hand back an acknowledgement.
fn leap_answer(received: &str) -> Value {
    leap_objects(received)
        .find(|v| v.get("CommuniqueType").and_then(Value::as_str) != Some("SubscribeResponse"))
        .unwrap_or(Value::Null)
}

#[derive(Default)]
pub struct CasetaLeap;

// ---------------------------------------------------------------------------------------
// Setup flow — pairing a bridge, then listing what is behind it.
// ---------------------------------------------------------------------------------------

fn field(state: &Value, key: &str) -> String {
    state.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// The same, for a value core handed back in `input`.
fn field_of(input: &Args, key: &str) -> String {
    input.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Merge a few keys into a state object, keeping everything else. State is opaque to core —
/// it round-trips through the installer's browser between steps, so it has to be plain JSON.
fn with_fields(state: &Value, updates: &[(&str, &str)]) -> Value {
    let mut m = state.as_object().cloned().unwrap_or_default();
    for (k, v) in updates {
        m.insert((*k).to_string(), json!(v));
    }
    Value::Object(m)
}

fn instruct(title: &str, body: &str) -> SetupStep {
    SetupStep::Instruct {
        title: title.into(),
        body: body.into(),
        continue_label: "Continue".into(),
    }
}

/// What core's mDNS scan found before this flow's first call, if anything — `(name, address)`.
fn mdns_candidates(state: &Value) -> Vec<(String, String)> {
    state
        .get("mdns_candidates")
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .filter_map(|c| {
                    let name = c.get("name").and_then(Value::as_str)?.to_string();
                    let address = c.get("address").and_then(Value::as_str)?.to_string();
                    Some((name, address))
                })
                .collect()
        })
        .unwrap_or_default()
}

impl CasetaLeap {
    fn ask_address(state: &Value, input: &Args) -> (SetupStep, Value) {
        let typed = input
            .get("Bridge address")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let found = mdns_candidates(state);

        // Exactly one bridge answered mDNS and nobody typed something else — that is the
        // bridge, no need to make anyone confirm an IP address they never had to know.
        if typed.is_empty() && found.len() == 1 {
            return Self::begin_pairing(state, &found[0].1);
        }
        if !typed.is_empty() {
            return Self::begin_pairing(state, &typed);
        }

        let body = if found.is_empty() {
            "Enter the bridge's IP address. You'll be asked to press the button on the \
             bridge next."
                .to_string()
        } else {
            format!(
                "Found {} on the network — pick the right one, or type an address. \
                 You'll be asked to press the button on the bridge next.",
                found.iter().map(|(n, a)| format!("{n} ({a})")).collect::<Vec<_>>().join(", ")
            )
        };
        (
            SetupStep::Form {
                title: "Pair a Caséta Smart Bridge".into(),
                body,
                fields: vec![Field {
                    name: "Bridge address".into(),
                    label: "Bridge IP address".into(),
                    kind: "string".into(),
                    help: "e.g. 192.168.1.50".into(),
                    default: found.first().map(|(_, a)| json!(a)),
                    options: Vec::new(),
                    required: true,
                }],
            },
            state.clone(),
        )
    }

    /// Ask core for the pairing identity, and remember where we are going.
    ///
    /// This used to call `rcgen` here. It cost 291 KB in a driver that is downloaded per
    /// project — `rcgen`, `ring` and seven crates behind them — for the four lines that made
    /// one certificate, every one of which core already had linked for its own TLS. Nothing a
    /// driver links is shared with anything; a `SetupStep` is. See `SetupStep::MakeIdentity`.
    fn begin_pairing(state: &Value, addr: &str) -> (SetupStep, Value) {
        (
            SetupStep::MakeIdentity {
                common_name: "juno".into(),
                note: String::new(),
            },
            with_fields(state, &[("stage", "making_identity"), ("address", addr)]),
        )
    }

    /// Core made the identity; hold on to it and get the installer ready to press the button.
    fn identity_made(state: &Value, input: &Args) -> (SetupStep, Value) {
        let (key_pem, csr_pem) = (field_of(input, "key_pem"), field_of(input, "csr_pem"));
        if key_pem.is_empty() || csr_pem.is_empty() {
            let why = field_of(input, "error");
            return (
                SetupStep::Failed {
                    reason: if why.is_empty() {
                        "could not generate a pairing key".into()
                    } else {
                        format!("could not generate a pairing key: {why}")
                    },
                },
                Value::Null,
            );
        }
        (
            instruct(
                "Ready to pair",
                "When you continue, this connects to the bridge and waits up to 30 \
                 seconds for it to confirm its button was pressed — so press and release \
                 the small button on top of the Caséta Smart Bridge right as you continue, \
                 not before.",
            ),
            with_fields(
                state,
                &[
                    ("stage", "ready_to_pair"),
                    ("key_pem", &key_pem),
                    ("csr_pem", &csr_pem),
                ],
            ),
        )
    }

    /// Open the pairing connection and listen, without sending anything yet.
    ///
    /// On real hardware, sending the CSR before the bridge has pushed its own confirmation
    /// that the button was physically pressed is answered with a handshake-level rejection,
    /// not a polite "not yet". So the connection is opened, held, and listened to first — and
    /// it has to be the *same* connection, because the push is not repeated for a new one.
    ///
    /// The connection presents Lutron's published pairing identity as its client certificate
    /// (see [`lap_identity`]); the per-installation certificate the bridge signs in response
    /// is what authorises anything afterward.
    fn send_pair_request(state: &Value) -> (SetupStep, Value) {
        let address = field(state, "address");
        (
            SetupStep::Session {
                session: None,
                open: Some(Connect::mutual_tls(
                    address,
                    PAIR_PORT,
                    lap_identity::LAP_CERT_PEM,
                    lap_identity::LAP_KEY_PEM,
                )),
                accept: None,
                send: String::new(),
                send_bytes: Vec::new(),
                read_ms: 1000,
                close: false,
                note: "pair".into(),
            },
            with_fields(state, &[("stage", "pairing"), ("waited", "0")]),
        )
    }

    /// The bridge pushes this once someone physically presses its button. Nothing else in the
    /// stream means the same thing, so this is the gate the CSR waits behind.
    fn button_pressed(received: &str) -> bool {
        leap_objects(received).any(|v| {
            v.pointer("/Body/Status/Permissions")
                .and_then(Value::as_array)
                .is_some_and(|p| p.iter().any(|x| x.as_str() == Some("PhysicalAccess")))
        })
    }

    /// Waiting on the button, on a connection that is already open. Poll until the push
    /// arrives, then write the CSR down the same connection.
    fn await_button_press(state: &Value, input: &Args) -> (SetupStep, Value) {
        if let Some(err) = input.get("error").and_then(Value::as_str) {
            return (
                instruct(
                    "Not paired yet",
                    &format!(
                        "{err} — press the button on the bridge right when you continue \
                         (the bridge only recognizes a press while a client is connected and \
                         waiting), then continue."
                    ),
                ),
                with_fields(state, &[("stage", "ready_to_pair")]),
            );
        }

        let session = input.get("session").and_then(Value::as_u64).map(|v| v as u32);
        let received = input.get("received").and_then(Value::as_str).unwrap_or("");
        let waited: u32 = field(state, "waited").parse().unwrap_or(0);

        if !Self::button_pressed(received) {
            if waited >= PAIR_WAIT_SECS {
                return (
                    instruct(
                        "No button press",
                        "The bridge did not report a press. Continue and press the button on \
                         the front of the bridge as soon as the next screen appears.",
                    ),
                    with_fields(state, &[("stage", "ready_to_pair")]),
                );
            }
            return (
                SetupStep::Session {
                    session,
                    open: None,
                    accept: None,
                    send: String::new(),
                    send_bytes: Vec::new(),
                    read_ms: 1000,
                    close: false,
                    note: "pair".into(),
                },
                with_fields(state, &[("waited", &(waited + 1).to_string())]),
            );
        }

        // Pressed. Send the signing request down the connection that saw the press, framed
        // the way a real bridge expects — matching `pylutron-caseta` field for field, since
        // that is the proven-working shape.
        let csr = field(state, "csr_pem");
        let body = json!({
            "Header": {
                "RequestType": "Execute",
                "Url": "/pair",
                "ClientTag": "get-cert",
            },
            "Body": {
                "CommandType": "CSR",
                "Parameters": {
                    "CSR": csr,
                    "DisplayName": "Juno",
                    "DeviceUID": "000000000000",
                    "Role": "Admin",
                },
            },
        });
        (
            SetupStep::Session {
                session,
                open: None,
                accept: None,
                send: leap_line(&body),
                send_bytes: Vec::new(),
                read_ms: 6000,
                close: true,
                note: "pair".into(),
            },
            with_fields(state, &[("stage", "pair_sent")]),
        )
    }

    fn handle_pair_response(state: &Value, input: &Args) -> (SetupStep, Value) {
        if let Some(err) = input.get("error").and_then(Value::as_str) {
            return (
                instruct(
                    "Not paired yet",
                    &format!(
                        "{err} — press the button on the bridge right when you continue \
                         (the bridge only recognizes a press while a client is connected and \
                         waiting), then continue."
                    ),
                ),
                with_fields(state, &[("stage", "ready_to_pair")]),
            );
        }
        let response = leap_answer(input.get("received").and_then(Value::as_str).unwrap_or(""));
        let cert = response
            .pointer("/Body/SigningResult/Certificate")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let ca = response
            .pointer("/Body/SigningResult/RootCertificate")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if cert.is_empty() || ca.is_empty() {
            return (
                SetupStep::Failed {
                    reason: format!("the bridge answered but sent no certificate: {response}"),
                },
                Value::Null,
            );
        }
        Self::request_device_list(&with_fields(
            state,
            &[("stage", "listing"), ("cert_pem", &cert), ("ca_pem", &ca)],
        ))
    }

    /// Address, client certificate and client key, wherever in `state` they currently live —
    /// under the bridge's own property names once adopted, under the pairing flow's names
    /// before that. Shared by every read this driver makes against the bridge.
    fn bridge_identity(state: &Value) -> (String, String, String) {
        let address = {
            let v = field(state, "Address");
            if v.is_empty() { field(state, "address") } else { v }
        };
        let cert = {
            let v = field(state, "Client certificate");
            if v.is_empty() { field(state, "cert_pem") } else { v }
        };
        let key = {
            let v = field(state, "Client key");
            if v.is_empty() { field(state, "key_pem") } else { v }
        };
        (address, cert, key)
    }

    fn request_device_list(state: &Value) -> (SetupStep, Value) {
        let (address, cert, key) = Self::bridge_identity(state);
        let body = json!({ "CommuniqueType": "ReadRequest", "Header": { "Url": "/device" } });
        (
            SetupStep::Session {
                session: None,
                open: Some(Connect::mutual_tls(address, LEAP_PORT, cert, key)),
                accept: None,
                send: leap_line(&body),
                send_bytes: Vec::new(),
                read_ms: 6000,
                // Held open: the answer may need another read (see `READ_RETRIES`), and the
                // `/buttongroup` read that follows goes down this same connection. Core closes
                // whatever is still open when the flow ends, so there is nothing to reap.
                close: false,
                note: "list".into(),
            },
            with_fields(state, &[("tries", "0")]),
        )
    }

    /// Listen again on the connection that was just asked something.
    ///
    /// `None` once the bridge has been given [`READ_RETRIES`] chances — at that point it is not
    /// answering, which is a failure to report rather than an empty list to draw.
    fn read_again(state: &Value, input: &Args, stage: &str) -> Option<(SetupStep, Value)> {
        let tries: u32 = field(state, "tries").parse().unwrap_or(0);
        if tries >= READ_RETRIES {
            return None;
        }
        Some((
            SetupStep::Session {
                session: input.get("session").and_then(Value::as_u64).map(|v| v as u32),
                open: None,
                accept: None,
                send: String::new(),
                send_bytes: Vec::new(),
                read_ms: 2000,
                close: false,
                note: "list".into(),
            },
            with_fields(state, &[("stage", stage), ("tries", &(tries + 1).to_string())]),
        ))
    }

    /// Turn a `/device` reply into candidates — or, if a Pico is behind this bridge, into one
    /// more read first. A key lives in `/button`, a separate collection `/device` only points
    /// at, so a Pico cannot be offered from this reply alone the way a dimmer can.
    ///
    /// Reads `received`/`leap_answer` rather than a pre-parsed `response`, the same as every
    /// other reply on this connection — `SetupStep::Session` never hands back the latter, only
    /// `SetupStep::Fetch` (an HTTP call this driver does not make) does.
    fn handle_device_list(state: &Value, input: &Args, include_bridge: bool) -> (SetupStep, Value) {
        if let Some(err) = input.get("error").and_then(Value::as_str) {
            return (
                SetupStep::Failed { reason: format!("could not read the bridge's device list: {err}") },
                Value::Null,
            );
        }
        let response = leap_answer(input.get("received").and_then(Value::as_str).unwrap_or(""));
        // Whether this was the answer at all, rather than how much of one it was: a bridge
        // always lists itself, so an absent `Devices` means the reply is still on its way and
        // an empty one would be a bridge that has genuinely forgotten itself.
        let Some(devices) = response.pointer("/Body/Devices").and_then(Value::as_array).cloned()
        else {
            return Self::read_again(state, input, "listing").unwrap_or((
                SetupStep::Failed { reason: "the bridge did not answer with its device list".into() },
                Value::Null,
            ));
        };

        let any_pico = devices
            .iter()
            .any(|d| is_pico(d.get("DeviceType").and_then(Value::as_str).unwrap_or("")));
        if any_pico {
            return Self::request_buttons(state, input, include_bridge, devices);
        }
        Self::request_areas(state, input, include_bridge, devices, Vec::new())
    }

    /// The names of the rooms the bridge knows, for the occupancy groups that follow: a group
    /// is identified by its area and by nothing else, so `/occupancygroup` alone would offer
    /// "Occupancy" three times with no way to tell which room each one watches.
    fn request_areas(
        state: &Value,
        input: &Args,
        include_bridge: bool,
        devices: Vec<Value>,
        buttons: Vec<Value>,
    ) -> (SetupStep, Value) {
        let mut next = with_fields(
            state,
            &[
                ("stage", "listing_areas"),
                ("include_bridge", if include_bridge { "true" } else { "false" }),
                ("tries", "0"),
            ],
        );
        if let Value::Object(ref mut m) = next {
            m.insert("devices_json".into(), Value::Array(devices));
            m.insert("buttons_json".into(), Value::Array(buttons));
        }
        (Self::read_on_this_connection(input, "/area"), next)
    }

    fn handle_areas(state: &Value, input: &Args, _include_bridge: bool) -> (SetupStep, Value) {
        if let Some(err) = input.get("error").and_then(Value::as_str) {
            return (
                SetupStep::Failed { reason: format!("could not read the bridge's areas: {err}") },
                Value::Null,
            );
        }
        let response = leap_answer(input.get("received").and_then(Value::as_str).unwrap_or(""));
        let Some(areas) = response.pointer("/Body/Areas").and_then(Value::as_array).cloned() else {
            return Self::read_again(state, input, "listing_areas").unwrap_or((
                SetupStep::Failed { reason: "the bridge did not answer with its areas".into() },
                Value::Null,
            ));
        };
        let mut next = with_fields(
            state,
            &[("stage", "listing_occupancy"), ("tries", "0")],
        );
        if let Value::Object(ref mut m) = next {
            m.insert("areas_json".into(), Value::Array(areas));
        }
        (Self::read_on_this_connection(input, "/occupancygroup"), next)
    }

    fn handle_occupancy(state: &Value, input: &Args, include_bridge: bool) -> (SetupStep, Value) {
        // A bridge with no occupancy at all still finishes: the devices found earlier are the
        // answer, and a failure here must not throw them away.
        let groups = input
            .get("error")
            .is_none()
            .then(|| {
                leap_answer(input.get("received").and_then(Value::as_str).unwrap_or(""))
                    .pointer("/Body/OccupancyGroups")
                    .and_then(Value::as_array)
                    .cloned()
            })
            .flatten();
        let Some(groups) = groups else {
            return match Self::read_again(state, input, "listing_occupancy") {
                Some(again) => again,
                None => (SetupStep::done(Self::all_candidates(state, include_bridge, &[])), Value::Null),
            };
        };
        (
            SetupStep::done(Self::all_candidates(state, include_bridge, &groups)),
            Value::Null,
        )
    }

    /// Everything the flow gathered, as one list. The devices and their buttons were carried
    /// through `state`; the areas and groups arrived last.
    fn all_candidates(state: &Value, include_bridge: bool, groups: &[Value]) -> Vec<Candidate> {
        let devices: Vec<Value> =
            state.get("devices_json").and_then(Value::as_array).cloned().unwrap_or_default();
        let buttons: Vec<Value> =
            state.get("buttons_json").and_then(Value::as_array).cloned().unwrap_or_default();
        let areas: Vec<Value> =
            state.get("areas_json").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut out = Self::candidates(state, include_bridge, &devices, &buttons);
        out.extend(occupancy_candidates(&areas, groups));
        out
    }

    /// A read down the connection this flow already has open.
    fn read_on_this_connection(input: &Args, url: &str) -> SetupStep {
        let body = json!({ "CommuniqueType": "ReadRequest", "Header": { "Url": url } });
        SetupStep::Session {
            session: input.get("session").and_then(Value::as_u64).map(|v| v as u32),
            open: None,
            accept: None,
            send: leap_line(&body),
            send_bytes: Vec::new(),
            read_ms: 6000,
            close: false,
            note: "list".into(),
        }
    }

    /// One more read, made only when something found in `/device` needs it — most bridges have
    /// no Pico behind them and never pay for this round trip.
    /// Every key on every remote behind this bridge, in one read.
    ///
    /// `/button` rather than `/buttongroup`, which is what this asked for until the button a key
    /// *is* turned out to matter: a group lists hrefs and nothing else, so the only way to tell
    /// which key an href was involved assuming its position in that list was its `ButtonNumber`.
    /// It is not — see `PICO_KEYS` — and on a remote with fewer keys than the group has entries
    /// that assumption names every one of them wrong. A button carries its own number, and its
    /// parent group, which is everything `/buttongroup` was being read for.
    fn request_buttons(
        state: &Value,
        input: &Args,
        include_bridge: bool,
        devices: Vec<Value>,
    ) -> (SetupStep, Value) {
        let body = json!({ "CommuniqueType": "ReadRequest", "Header": { "Url": "/button" } });

        let mut next = with_fields(
            state,
            &[
                ("stage", "listing_buttons"),
                ("include_bridge", if include_bridge { "true" } else { "false" }),
                // A fresh budget for the second question, not what the first one had left.
                ("tries", "0"),
            ],
        );
        if let Value::Object(ref mut m) = next {
            // Carried across this round trip as plain JSON, the same as everything else in
            // `state` — discarded the moment `candidates` below has consumed it.
            m.insert("devices_json".into(), Value::Array(devices));
        }
        (
            SetupStep::Session {
                // Down the connection that just answered `/device` rather than a second
                // handshake: the bridge only volunteers its acknowledgements once per
                // connection, so reusing this one is also one fewer round of them to read past.
                session: input.get("session").and_then(Value::as_u64).map(|v| v as u32),
                open: None,
                accept: None,
                send: leap_line(&body),
                send_bytes: Vec::new(),
                read_ms: 6000,
                close: false,
                note: "list".into(),
            },
            next,
        )
    }

    fn handle_buttons(state: &Value, input: &Args, include_bridge: bool) -> (SetupStep, Value) {
        if let Some(err) = input.get("error").and_then(Value::as_str) {
            return (
                SetupStep::Failed { reason: format!("could not read the bridge's buttons: {err}") },
                Value::Null,
            );
        }
        let response = leap_answer(input.get("received").and_then(Value::as_str).unwrap_or(""));
        let Some(buttons) = response.pointer("/Body/Buttons").and_then(Value::as_array).cloned()
        else {
            return Self::read_again(state, input, "listing_buttons").unwrap_or((
                SetupStep::Failed { reason: "the bridge did not answer with its buttons".into() },
                Value::Null,
            ));
        };
        let devices: Vec<Value> = state.get("devices_json").and_then(Value::as_array).cloned().unwrap_or_default();
        Self::request_areas(state, input, include_bridge, devices, buttons)
    }

    fn candidates(state: &Value, include_bridge: bool, devices: &[Value], buttons: &[Value]) -> Vec<Candidate> {
        let mut out = Vec::new();
        if include_bridge {
            let mut props = BTreeMap::new();
            props.insert("Address".into(), json!(field(state, "address")));
            props.insert("Client certificate".into(), json!(field(state, "cert_pem")));
            props.insert("Client key".into(), json!(field(state, "key_pem")));
            props.insert("CA certificate".into(), json!(field(state, "ca_pem")));
            out.push(Candidate {
                label: "Caséta Smart Bridge".into(),
                kind: "bridge".into(),
                driver_id: BRIDGE_ID.into(),
                properties: props,
                verified: "paired".into(),
                ..Default::default()
            });
        }

        // ponytail: occupancy sensors are the one LEAP group still missing. They are not zones
        // — they arrive through `/occupancygroup`, which is a different collection and a
        // different subscription — so they want their own read rather than an arm here.
        for d in devices {
            let kind = d.get("DeviceType").and_then(Value::as_str).unwrap_or("");
            let (name, room) = caseta_name(d);

            // Every zone behind the bridge, whatever it turns out to drive. They are told apart
            // only by `DeviceType` — the zone itself looks identical for all of them — so the
            // answer is written down as `Kind` and read back on every command afterwards.
            let zoned = if DIMMABLE.contains(&kind) {
                Some((Kind::Light, DIMMER_ID, "light"))
            } else if SWITCHED.contains(&kind) {
                Some((Kind::Switch, SWITCH_ID, "switch"))
            } else if FANS.contains(&kind) {
                Some((Kind::Fan, FAN_ID, "fan"))
            } else if COVERS.contains(&kind) {
                Some((Kind::Shade, SHADE_ID, "blind"))
            } else {
                None
            };
            if let Some((which, driver_id, proxy)) = zoned {
                let Some(zone) = d.pointer("/LocalZones/0/href").and_then(Value::as_str) else {
                    continue; // no zone means nothing to command
                };
                let mut props = BTreeMap::new();
                props.insert("Zone".into(), json!(zone));
                props.insert("Device href".into(), json!(d.get("href").and_then(Value::as_str).unwrap_or("")));
                props.insert("Kind".into(), json!(which.as_str()));
                out.push(Candidate {
                    label: name,
                    room,
                    kind: proxy.into(),
                    driver_id: driver_id.into(),
                    properties: props,
                    // Slats or not, which only the model knows — a roller shade offering a tilt
                    // control is a control that does nothing.
                    capabilities: if which == Kind::Shade {
                        BTreeMap::from([("supports_tilt".to_string(), json!(kind.contains("Tilt")))])
                    } else {
                        BTreeMap::new()
                    },
                    verified: "found on bridge".into(),
                    ..Default::default()
                });
                continue;
            }

            if is_pico(kind) {
                let keys = pico_keys_of(d, buttons);
                if keys.is_empty() {
                    continue; // no buttons found for it means nothing to subscribe to
                }
                let mut props = BTreeMap::new();
                // The remote itself, not one of its keys. `/device/<n>/status` is where the
                // battery lives, and a driver holding five button hrefs still had no way to
                // name the thing they are on.
                props.insert("Device href".into(), json!(d.get("href").and_then(Value::as_str).unwrap_or("")));
                for (i, (href, _)) in keys.iter().enumerate() {
                    props.insert(format!("Button {} href", i + 1), json!(href));
                }
                let labels: Vec<&str> = keys.iter().map(|(_, label)| label.as_str()).collect();
                out.push(Candidate {
                    label: name,
                    room,
                    kind: "keypad".into(),
                    driver_id: PICO_ID.into(),
                    properties: props,
                    capabilities: pico_capabilities(&labels),
                    verified: "found on bridge".into(),
                    ..Default::default()
                });
            }
        }

        out
    }
}

/// What the Caséta app calls this device, and which of its areas it sits in.
///
/// `FullyQualifiedName` is both, in one field: the path of areas a device is filed under with
/// its own name on the end — `["Kitchen", "Pico"]`. So the name is the last element and the
/// room is the one before it, which is the innermost area rather than the outermost. `Name`
/// alone is the same leaf and is what this falls back to; the areas are what somebody sat down
/// in the app and filed, and throwing them away means filing a whole house a second time.
///
/// Cross-checked against `/area` on real hardware rather than assumed: `/device/6` carries
/// `AssociatedArea: /area/4`, and `/area/4` is `Kitchen`, which is what the path says too. That
/// agreement is why this does not spend a third read resolving the href.
///
/// The room is a *suggestion* — see `Candidate::room`. Core matches or creates one at the
/// moment somebody adopts, with the list in front of them, and never for a bridge: a hub lives
/// in a cupboard and the cupboard is not a room.
fn caseta_name(device: &Value) -> (String, String) {
    let path: Vec<&str> = device
        .get("FullyQualifiedName")
        .and_then(Value::as_array)
        .map(|parts| parts.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let name = path
        .last()
        .copied()
        .or_else(|| device.get("Name").and_then(Value::as_str))
        .unwrap_or("Caséta device")
        .to_string();
    // Nothing but a name means the bridge files it at the top of the project, under no area at
    // all — which is an answer, and not one to guess past.
    let room = if path.len() >= 2 { path[path.len() - 2] } else { "" };
    (name, room.to_string())
}

/// The areas that have somebody watching them, as sensors.
///
/// A group with no `AssociatedSensors` is an area Lutron made a slot for and nobody put a
/// sensor in — every Caséta bridge has one per room whether or not anything reports. Offering
/// those would put a motion sensor in the house for every room the installer ever named, none
/// of which would ever fire.
fn occupancy_candidates(areas: &[Value], groups: &[Value]) -> Vec<Candidate> {
    let named = |href: &str| {
        areas
            .iter()
            .find(|a| a.get("href").and_then(Value::as_str) == Some(href))
            .and_then(|a| a.get("Name").and_then(Value::as_str))
            .unwrap_or("")
            .to_string()
    };

    groups
        .iter()
        .filter(|g| {
            g.get("AssociatedSensors")
                .and_then(Value::as_array)
                .is_some_and(|sensors| !sensors.is_empty())
        })
        .filter_map(|g| {
            let href = g.get("href").and_then(Value::as_str)?;
            let area = g
                .pointer("/AssociatedAreas/0/Area/href")
                .and_then(Value::as_str)
                .map(named)
                .unwrap_or_default();
            // "Kitchen Occupancy", the way `pylutron-caseta` names them, because a house with
            // four of these needs to be able to tell them apart in a list of triggers.
            let label = if area.is_empty() {
                "Occupancy".to_string()
            } else {
                format!("{area} Occupancy")
            };
            Some(Candidate {
                label,
                room: area,
                kind: "sensor".into(),
                driver_id: OCCUPANCY_ID.into(),
                properties: BTreeMap::from([("Occupancy href".to_string(), json!(href))]),
                verified: "found on bridge".into(),
                ..Default::default()
            })
        })
        .collect()
}

/// Any Pico remote — `Pico2Button`, `Pico3ButtonRaiseLower`, `Pico4Button` and the rest all
/// start with it, and this driver treats every one of them the same way (see `PICO_ID`'s
/// manifest for the ceiling that puts a real name on).
fn is_pico(device_type: &str) -> bool {
    device_type.starts_with("Pico")
}

/// This Pico's keys: each one's href and what it is called, in `ButtonNumber` order.
///
/// The manifest cannot answer it. "Pico" is a family, not a product: a Pico2Button has two keys
/// and a Pico3ButtonRaiseLower has five, and one manifest covers both because they differ in
/// nothing else. Declaring the largest meant a two-button remote arrived with five keys that
/// were drawn in the UI, offered in the automation editor and impossible to press — the same
/// mistake a four-HDMI declaration makes on a three-port television, and core has the same
/// answer for it: the driver knows, so the driver says. See `Candidate::capabilities`.
///
/// What the driver knows is `PICO_KEYS`, because the bridge does not: it lists five buttons
/// under a remote that has two, names every one of them `Button N`, and leaves the model as the
/// only thing that says which is which. Counting what came back is what claimed five keys on a
/// four-key remote. A model that is not in the table keeps that behaviour deliberately — every
/// key the bridge listed, numbered — since `Button 3` that presses beats `Off` that does not.
fn pico_keys_of(device: &Value, buttons: &[Value]) -> Vec<(String, String)> {
    let groups: Vec<&str> = device
        .get("ButtonGroups")
        .and_then(Value::as_array)
        .map(|groups| {
            groups
                .iter()
                .filter_map(|group| group.get("href").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();

    // This remote's keys, by the number the bridge gave each — the identity a model's table is
    // written against. Sorted, because the numbered fallback below reads position as order.
    let mut mine: Vec<(u64, &str)> = buttons
        .iter()
        .filter(|button| {
            button
                .pointer("/Parent/href")
                .and_then(Value::as_str)
                .is_some_and(|parent| groups.contains(&parent))
        })
        .filter_map(|button| {
            Some((
                button.get("ButtonNumber").and_then(Value::as_u64)?,
                button.get("href").and_then(Value::as_str)?,
            ))
        })
        .collect();
    mine.sort_by_key(|(number, _)| *number);

    let model = device.get("DeviceType").and_then(Value::as_str).unwrap_or("");
    match PICO_KEYS.iter().find(|(known, _)| *known == model) {
        Some((_, keys)) => keys
            .iter()
            .take(MAX_BUTTONS)
            // A number the table names and this remote did not report is a key that is not
            // there. Skipped rather than counted, which is the whole point of the table.
            .filter_map(|(number, label)| {
                let href = mine.iter().find(|(theirs, _)| theirs == number)?.1;
                Some((href.to_string(), (*label).to_string()))
            })
            .collect(),
        None => mine
            .iter()
            .take(MAX_BUTTONS)
            .enumerate()
            .map(|(i, (_, href))| (href.to_string(), format!("Button {}", i + 1)))
            .collect(),
    }
}

/// What a Pico's keypad proxy claims, for the candidate that is about to be adopted.
fn pico_capabilities(labels: &[&str]) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("key_count".to_string(), json!(labels.len())),
        ("key_labels".to_string(), json!(labels.join(","))),
        // A Pico runs on a coin cell and the bridge knows how it is doing. Declared because
        // `battery_changed` requires it — see the keypad contract.
        ("has_battery".to_string(), json!(true)),
        // And it can tell a hold from a click, now that a driver can ask to be woken — see
        // `HOLD_MS`. Lutron reports the press and the release and leaves the line between them
        // to whoever is listening; this is the listener drawing it. Declaring it is what puts
        // Hold and Release in the link editor and lets a key run a lamp up while it is down.
        ("has_hold".to_string(), json!(true)),
    ])
}

impl DriverModule for CasetaLeap {
    fn discover(&self, driver_id: &str, state: &Value, input: &Args) -> (SetupStep, Value) {
        if driver_id != BRIDGE_ID {
            // Dimmers are found by browsing their bridge, never set up on their own.
            return (SetupStep::done(Vec::new()), Value::Null);
        }

        // Browsing a bridge that is paired already: core seeded `state` with its properties
        // directly, so there is nothing to pair — go straight to listing, then whatever
        // Picos found there need one more read for their buttons, same as first-time pairing.
        if state.get("browse").and_then(Value::as_bool) == Some(true) {
            return match field(state, "stage").as_str() {
                "listing" => Self::handle_device_list(state, input, false),
                "listing_buttons" => Self::handle_buttons(state, input, false),
                "listing_areas" => Self::handle_areas(state, input, false),
                "listing_occupancy" => Self::handle_occupancy(state, input, false),
                _ => Self::request_device_list(&with_fields(state, &[("stage", "listing")])),
            };
        }

        match field(state, "stage").as_str() {
            "making_identity" => Self::identity_made(state, input),
            "ready_to_pair" => Self::send_pair_request(state),
            // Pairing is two stages on one held connection: wait for the bridge to report the
            // button press, then send the signing request down the same socket.
            "pairing" => Self::await_button_press(state, input),
            "pair_sent" => Self::handle_pair_response(state, input),
            "listing" => Self::handle_device_list(state, input, true),
            "listing_buttons" => Self::handle_buttons(state, input, true),
            "listing_areas" => Self::handle_areas(state, input, true),
            "listing_occupancy" => Self::handle_occupancy(state, input, true),
            _ => Self::ask_address(state, input),
        }
    }

    fn on_command(&self, inst: &mut Instance, _proxy: LocalId, cmd: &str, args: &Args) -> Vec<HostCall> {
        // The bridge and a Pico's keypad proxy both take no commands (see their manifests), so
        // everything arriving here is for a zone — and which zone it is decides both what to
        // send and what to say afterwards. See `Kind`.
        match Kind::of(inst) {
            Kind::Light => Self::on_dimmer_command(inst, cmd, args),
            Kind::Switch => Self::on_switch_command(inst, cmd),
            Kind::Fan => Self::on_fan_command(inst, cmd, args),
            Kind::Shade => Self::on_shade_command(inst, cmd, args),
        }
    }

    fn on_event(&self, inst: &mut Instance, _control: LocalId, note: &str, args: &Args) -> Vec<HostCall> {
        if !pico_buttons(inst).is_empty() {
            return Self::on_pico_event(inst, note, args);
        }
        // An occupancy group is not a zone and has no level; it is told apart the same way a
        // Pico is, by the property only it carries.
        if let Some(group) = occupancy_href(inst) {
            return Self::on_occupancy_event(&group, note, args);
        }
        // The bridge answering the one question it was asked at bind.
        if zone(inst).is_none() {
            return Self::on_bridge_event(note, args);
        }
        Self::on_dimmer_event(inst, note, args)
    }

    /// Provider-owned scenes, handed to core as borrowed handles. Recall is all they support and
    /// all they should: what a virtual button does was decided in the Caséta app, and this
    /// driver has no way to read it back, let alone write it.
    fn on_scene(&self, _inst: &mut Instance, request: &SceneRequest) -> SceneResponse {
        match &request.operation {
            SceneOperation::Recall { .. } => match request.resource.as_deref() {
                Some(resource) if resource.starts_with("/virtualbutton/") => SceneResponse {
                    calls: vec![tx(&press_virtual_button(resource))],
                    ..Default::default()
                },
                _ => SceneResponse {
                    problem: Some("caseta-leap: not a Caséta scene".into()),
                    ..Default::default()
                },
            },
            // Nothing here owns a Caséta scene, so there is nothing to create, change or
            // detach. Saying so is the honest answer; pretending would put a Juno-owned scene
            // in an app that has never heard of Juno.
            _ => SceneResponse {
                problem: Some(
                    "caseta-leap: Caséta scenes are programmed in the Caséta app and can only be \
                     recalled from here"
                        .into(),
                ),
                ..Default::default()
            },
        }
    }

    fn on_bind(&self, inst: &mut Instance) -> Vec<HostCall> {
        let mut a = Args::new();
        a.insert("online".into(), json!(true));
        let mut out = vec![HostCall::notify(1, "online_changed", a)];

        // A dimmer has a zone to subscribe to; a Pico has buttons instead; a bridge instance
        // binding has nothing further to do here — pairing already happened in its setup flow.
        if let Some(z) = zone(inst) {
            out.push(tx(&subscribe(&z)));
        }
        // An area's occupancy. One subscription covers every group on the bridge, and each
        // device filters the feed down to its own — see `on_occupancy_event`.
        if occupancy_href(inst).is_some() {
            out.push(tx(&subscribe_occupancy()));
            return out;
        }

        // The bridge itself. It has no zone and no buttons; what it has is the house's scenes.
        if inst.property("Client certificate").as_str().is_some_and(|c| !c.is_empty())
            && zone(inst).is_none()
            && pico_buttons(inst).is_empty()
        {
            out.push(tx(&read_virtual_buttons()));
            return out;
        }

        let buttons = pico_buttons(inst);
        for (_, href) in &buttons {
            out.push(tx(&subscribe_button(href)));
        }
        // And how its battery is doing, for the one kind of device here that has one. Asked
        // once on binding rather than polled: a coin cell does not move in an afternoon, and
        // the answer arrives on the same connection as everything else.
        if !buttons.is_empty()
            && let Some(href) = device_href(inst)
        {
            out.push(tx(&read_status(&href)));
        }
        out
    }
}

// ---------------------------------------------------------------------------------------
// The live connection — one zone's on/off/dim, and the status push that keeps it in sync.
// ---------------------------------------------------------------------------------------

/// This device's own href on the bridge — `/device/6`. Written at adoption; absent on anything
/// adopted before this driver started asking for it, which is why every use of it is optional
/// rather than assumed.
/// One key's hold clock, and the flag that says it went off. The same string does both, so
/// there is no second place for them to disagree.
fn held_note(key: u64) -> String {
    format!("held:{key}")
}

/// Whether this key crossed into a hold. Read on release to decide what the press turned out
/// to be — a hold that ended, or a click that never became one.
fn held_flag(inst: &Instance, key: u64) -> bool {
    inst.scratch.get(&held_note(key)).and_then(Value::as_bool) == Some(true)
}

/// The occupancy group this device is, when it is one — `/occupancygroup/2`. Present only on an
/// occupancy device, which is how everything else tells itself apart from one.
fn occupancy_href(inst: &Instance) -> Option<String> {
    inst.property("Occupancy href")
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Ask for every group's occupancy, and keep hearing about them.
fn subscribe_occupancy() -> Value {
    json!({ "CommuniqueType": "SubscribeRequest", "Header": { "Url": "/occupancygroup/status" } })
}

fn device_href(inst: &Instance) -> Option<String> {
    inst.property("Device href")
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Ask for a device's own status. The battery lives here and nowhere else.
fn read_status(href: &str) -> Value {
    json!({ "CommuniqueType": "ReadRequest", "Header": { "Url": format!("{href}/status") } })
}

/// What the bridge's coarse battery level is worth as the percentage the contract asks for.
///
/// Lutron reports a word, not a number: `Good` or `Low`. The keypad contract wants a `u8`, so
/// something has to be invented, and the honest invention is one that reads correctly on a
/// battery gauge — full, or nearly empty and worth acting on. Anything unrecognised reports
/// nothing at all rather than a number nobody can stand behind.
fn battery_percent(level: &str) -> Option<u8> {
    match level {
        "Good" | "Normal" | "Full" => Some(100),
        "Low" => Some(10),
        "Critical" => Some(2),
        _ => None,
    }
}

/// The one complaint worth making when a zone device has no zone: it was adopted by hand
/// rather than through the bridge that knows its hrefs. Four command handlers now open with the
/// same two lines, and this is the half of them that is worth saying once.
fn unzoned() -> Vec<HostCall> {
    vec![HostCall::warn(
        "caseta-leap: this device has no Zone — adopt it through the bridge's setup flow",
    )]
}

fn zone(inst: &Instance) -> Option<String> {
    inst.property("Zone")
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// This instance's button hrefs, numbered as its manifest properties are — empty for
/// anything that is not a Pico, which is how a bridge or a dimmer binding tells `on_event`
/// and `on_bind` it is neither.
fn pico_buttons(inst: &Instance) -> Vec<(u64, String)> {
    (1..=MAX_BUTTONS as u64)
        .filter_map(|n| {
            let href = inst.property(&format!("Button {n} href")).as_str()?;
            (!href.is_empty()).then(|| (n, href.to_string()))
        })
        .collect()
}

fn fade_time(seconds: u64) -> String {
    format!("{:02}:{:02}:{:02}", seconds / 3600, (seconds % 3600) / 60, seconds % 60)
}

fn tx(msg: &Value) -> HostCall {
    let mut line = msg.to_string();
    line.push('\n');
    HostCall::Tx { control: NET, data: line.into_bytes() }
}

fn go_to_level(zone: &str, level: u8, fade_secs: u64) -> Value {
    json!({
        "CommuniqueType": "CreateRequest",
        "Header": { "Url": format!("{zone}/commandprocessor") },
        "Body": {
            "Command": {
                "CommandType": "GoToDimmedLevel",
                "DimmedLevelParameters": { "Level": level, "FadeTime": fade_time(fade_secs) },
            },
        },
    })
}

/// A plain level, with no fade. What a switch takes — it has no dimmer to fade — and what a
/// shade takes for a position, since `GoToDimmedLevel` is a lighting command and a motor does
/// not accept one.
fn go_to_plain_level(zone: &str, level: u8) -> Value {
    zone_command(zone, json!({
        "CommandType": "GoToLevel",
        "Parameter": [{ "Type": "Level", "Value": level }],
    }))
}

/// The speed, in Lutron's spelling. `None` for a name this driver does not know, which is a
/// rule asking for a speed the fan does not have rather than something to guess at.
fn go_to_fan_speed(zone: &str, speed: &str) -> Option<Value> {
    let theirs = FAN_SPEEDS.iter().find(|(ours, _)| *ours == speed).map(|(_, theirs)| *theirs)?;
    Some(zone_command(zone, json!({
        "CommandType": "GoToFanSpeed",
        "FanSpeedParameters": { "FanSpeed": theirs },
    })))
}

/// `Raise`, `Lower` and `Stop` take no parameters at all — a shade already knows which way it
/// is allowed to go.
fn shade_move(zone: &str, command: &str) -> Value {
    zone_command(zone, json!({ "CommandType": command }))
}

fn go_to_tilt(zone: &str, tilt: u8) -> Value {
    zone_command(zone, json!({
        "CommandType": "GoToTilt",
        "TiltParameters": { "Tilt": tilt },
    }))
}

/// Everything above, wrapped the one way the bridge accepts a command.
fn zone_command(zone: &str, command: Value) -> Value {
    json!({
        "CommuniqueType": "CreateRequest",
        "Header": { "Url": format!("{zone}/commandprocessor") },
        "Body": { "Command": command },
    })
}

/// Every scene the bridge holds. A Caséta scene is a "virtual button": programmed in the app,
/// pressed by anything that can reach the bridge.
fn read_virtual_buttons() -> Value {
    json!({ "CommuniqueType": "ReadRequest", "Header": { "Url": "/virtualbutton" } })
}

/// Recall one. There is no "set this scene to these levels" — a virtual button is pressed, and
/// what it does was decided in the Caséta app. That is exactly what a *borrowed* scene is.
fn press_virtual_button(resource: &str) -> Value {
    json!({
        "CommuniqueType": "CreateRequest",
        "Header": { "Url": format!("{resource}/commandprocessor") },
        "Body": { "Command": { "CommandType": "PressAndRelease" } },
    })
}

fn subscribe(zone: &str) -> Value {
    json!({ "CommuniqueType": "SubscribeRequest", "Header": { "Url": format!("{zone}/status") } })
}

fn subscribe_button(href: &str) -> Value {
    json!({ "CommuniqueType": "SubscribeRequest", "Header": { "Url": format!("{href}/status/event") } })
}

fn report(level: u8) -> HostCall {
    let mut args = Args::new();
    args.insert("level".into(), json!(level));
    HostCall::notify(1, "level_changed", args)
}

impl CasetaLeap {
    /// A switched load. `GoToLevel` and not `GoToDimmedLevel`: there is no dimmer to fade, and
    /// the level is the only two values it holds.
    fn on_switch_command(inst: &mut Instance, cmd: &str) -> Vec<HostCall> {
        let Some(z) = zone(inst) else { return unzoned() };
        let on = match cmd {
            "on" => true,
            "off" => false,
            // What it is now, inverted. The bridge has no toggle, and a driver that guessed
            // would turn a lamp on that somebody had just turned off.
            "toggle" => !inst.scratch.get("on").and_then(Value::as_bool).unwrap_or(false),
            _ => return Vec::new(),
        };
        vec![tx(&go_to_plain_level(&z, if on { 100 } else { 0 }))]
    }

    fn on_fan_command(inst: &mut Instance, cmd: &str, args: &Args) -> Vec<HostCall> {
        let Some(z) = zone(inst) else { return unzoned() };
        match cmd {
            "off" => vec![tx(&go_to_fan_speed(&z, "off").unwrap_or_else(|| {
                // `Off` is not in `FAN_SPEEDS` — stopping is a command, not a speed — so it is
                // spelled out here rather than smuggled into the speed table.
                zone_command(&z, json!({
                    "CommandType": "GoToFanSpeed",
                    "FanSpeedParameters": { "FanSpeed": "Off" },
                }))
            }))],
            // The speed it was last on. A fan turned on to a speed nobody chose is a fan that
            // comes back at full in the middle of the night.
            "on" => {
                let last = inst
                    .scratch
                    .get("speed")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| FAN_SPEEDS[0].0.to_string());
                go_to_fan_speed(&z, &last).map(|m| vec![tx(&m)]).unwrap_or_default()
            }
            "toggle" => {
                let running = inst.scratch.get("on").and_then(Value::as_bool).unwrap_or(false);
                let cmd = if running { "off" } else { "on" };
                Self::on_fan_command(inst, cmd, args)
            }
            "set_speed" => {
                let Some(speed) = args.get("speed").and_then(Value::as_str) else {
                    return Vec::new();
                };
                go_to_fan_speed(&z, speed).map(|m| vec![tx(&m)]).unwrap_or_else(|| {
                    vec![HostCall::warn(&format!(
                        "caseta-leap: this fan has no speed called `{speed}`"
                    ))]
                })
            }
            _ => Vec::new(),
        }
    }

    /// A shade. Open and close are the ends of its travel rather than commands of their own,
    /// which is why they are levels; `Raise`, `Lower` and `Stop` are the motor's own verbs and
    /// take no parameters.
    fn on_shade_command(inst: &mut Instance, cmd: &str, args: &Args) -> Vec<HostCall> {
        let Some(z) = zone(inst) else { return unzoned() };
        match cmd {
            "open" => vec![tx(&go_to_plain_level(&z, 100))],
            "close" => vec![tx(&go_to_plain_level(&z, 0))],
            "stop" => vec![tx(&shade_move(&z, "Stop"))],
            "set_position" => match args.get("position").and_then(Value::as_u64) {
                Some(p) => vec![tx(&go_to_plain_level(&z, p.min(100) as u8))],
                None => Vec::new(),
            },
            "set_tilt" => match args.get("tilt").and_then(Value::as_u64) {
                Some(t) => vec![tx(&go_to_tilt(&z, t.min(100) as u8))],
                None => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    fn on_dimmer_command(inst: &mut Instance, cmd: &str, args: &Args) -> Vec<HostCall> {
        let Some(z) = zone(inst) else {
            return vec![HostCall::warn(
                "caseta-leap: this device has no Zone — adopt it through the bridge's setup flow",
            )];
        };

        let default_fade = inst.property("Default fade").as_u64().unwrap_or(1);
        let secs = args
            .get("ramp_ms")
            .and_then(Value::as_u64)
            .map(|ms| ms / 1000)
            .unwrap_or(default_fade);
        let last = inst.scratch.get("level").and_then(Value::as_u64).unwrap_or(100) as u8;

        // LEAP has real open-ended raise/lower/stop commands (`CommandType: "Raise"` etc.);
        // ponytail: this fakes a held button with a long fade to the extreme instead, the way
        // the Hue driver does, since it needs no separate stop-tracking state. Swap in the
        // native commands if a held keypad button needs to feel less like a slow fade.
        let (level, fade) = match cmd {
            "on" => (if last == 0 { 100 } else { last }, secs),
            "off" => (0, secs),
            "toggle" => {
                let on = inst.scratch.get("on").and_then(Value::as_bool).unwrap_or(false);
                (if on { 0 } else if last == 0 { 100 } else { last }, secs)
            }
            "set_level" => (args.get("level").and_then(Value::as_u64).unwrap_or(0) as u8, secs),
            // Held on a keypad: fade toward the end of the travel and wait to be stopped. The
            // level here is a destination, not a state, and the two must not be confused — this
            // used to write 100 into `scratch` and report it, so the house believed the lamp was
            // already at full the instant somebody touched the button, and `ramp_stop` then
            // "stopped" it by jumping to the 100 it had invented. A hold that ended early left
            // the light at exactly the brightness the driver had lied about.
            //
            // So a ramp reports nothing and remembers nothing. The bridge pushes the zone's
            // level as it moves, and that is what the house learns from — see `on_dimmer_event`.
            "ramp_start" => {
                let up = args.get("direction").and_then(Value::as_str) == Some("up");
                return vec![tx(&go_to_level(&z, if up { 100 } else { 1 }, RAMP_SECS))];
            }
            // Where it actually got to, held there. `last` is the newest level the *bridge*
            // reported, which during a fade is where the light is rather than where it was sent.
            "ramp_stop" => return vec![tx(&go_to_level(&z, last, 0))],
            other => return vec![HostCall::warn(format!("caseta-leap: unhandled `{other}`"))],
        };

        if level > 0 {
            inst.scratch.insert("level".into(), json!(level));
        }
        inst.scratch.insert("on".into(), json!(level > 0));

        vec![tx(&go_to_level(&z, level, fade)), report(level)]
    }

    fn on_dimmer_event(inst: &mut Instance, note: &str, args: &Args) -> Vec<HostCall> {
        if note != "rx" {
            return Vec::new();
        }
        let Some(text) = args.get("data").and_then(Value::as_str) else {
            return Vec::new();
        };
        let Some(mine) = zone(inst) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        for line in text.split('\n').map(str::trim).filter(|l| !l.is_empty()) {
            let Ok(msg) = driver_sdk::serde_json::from_str::<Value>(line) else { continue };
            let Some(status) = msg.pointer("/Body/ZoneStatus") else { continue };
            let href = status.pointer("/Zone/href").and_then(Value::as_str).unwrap_or("");
            if href != mine {
                continue;
            }
            // What a zone reports depends on what is on the end of it, and so does what this
            // owes upward: the same `ZoneStatus` carries a fan's speed, a shade's tilt and a
            // dimmer's level, and the three proxies do not share a notification between them.
            match Kind::of(inst) {
                Kind::Fan => {
                    let Some(theirs) = status.get("FanSpeed").and_then(Value::as_str) else {
                        continue;
                    };
                    let ours = FAN_SPEEDS
                        .iter()
                        .find(|(_, t)| *t == theirs)
                        .map(|(ours, _)| *ours);
                    let running = ours.is_some();
                    if inst.scratch.get("speed").and_then(Value::as_str) == ours
                        && inst.scratch.get("on").and_then(Value::as_bool) == Some(running)
                    {
                        continue; // already knew
                    }
                    // A speed it is running at is remembered; `Off` is not, so that turning it
                    // back on returns it to what somebody chose rather than to a stop.
                    if let Some(speed) = ours {
                        inst.scratch.insert("speed".into(), json!(speed));
                    }
                    inst.scratch.insert("on".into(), json!(running));
                    let mut a = Args::new();
                    a.insert("speed".into(), json!(ours.unwrap_or("off")));
                    a.insert("on".into(), json!(running));
                    out.push(HostCall::notify(1, "speed_changed", a));
                    continue;
                }
                Kind::Shade => {
                    if let Some(tilt) = status.get("Tilt").and_then(Value::as_u64) {
                        let tilt = tilt.min(100) as u8;
                        if inst.scratch.get("tilt").and_then(Value::as_u64) != Some(tilt as u64) {
                            inst.scratch.insert("tilt".into(), json!(tilt));
                            let mut a = Args::new();
                            a.insert("tilt".into(), json!(tilt));
                            out.push(HostCall::notify(1, "tilt_changed", a));
                        }
                    }
                    let Some(level) = status.get("Level").and_then(Value::as_u64) else { continue };
                    let position = level.min(100) as u8;
                    if inst.scratch.get("level").and_then(Value::as_u64) == Some(position as u64) {
                        continue;
                    }
                    inst.scratch.insert("level".into(), json!(position));
                    let mut a = Args::new();
                    a.insert("position".into(), json!(position));
                    out.push(HostCall::notify(1, "position_changed", a));
                    continue;
                }
                Kind::Switch => {
                    let Some(level) = status.get("Level").and_then(Value::as_u64) else { continue };
                    let on = level > 0;
                    if inst.scratch.get("on").and_then(Value::as_bool) == Some(on) {
                        continue;
                    }
                    inst.scratch.insert("on".into(), json!(on));
                    let mut a = Args::new();
                    a.insert("on".into(), json!(on));
                    out.push(HostCall::notify(1, "switch_changed", a));
                    continue;
                }
                Kind::Light => {}
            }

            let Some(level) = status.get("Level").and_then(Value::as_u64) else { continue };
            let level = level.clamp(0, 100) as u8;

            if inst.scratch.get("level").and_then(Value::as_u64) == Some(level as u64) {
                continue; // already knew; do not manufacture a state change
            }
            if level > 0 {
                inst.scratch.insert("level".into(), json!(level));
            }
            inst.scratch.insert("on".into(), json!(level > 0));
            out.push(report(level));
        }
        out
    }

    /// What the bridge itself hears: the scene list it asked for at bind.
    ///
    /// Filtered on `IsProgrammed`, because the bridge keeps fifty virtual buttons whether or not
    /// anybody has put anything on them — offering `Button 37` as a scene is offering a switch
    /// wired to nothing.
    fn on_bridge_event(note: &str, args: &Args) -> Vec<HostCall> {
        if note != "rx" {
            return Vec::new();
        }
        let Some(text) = args.get("data").and_then(Value::as_str) else {
            return Vec::new();
        };
        for line in text.split('\n').map(str::trim).filter(|l| !l.is_empty()) {
            let Ok(msg) = driver_sdk::serde_json::from_str::<Value>(line) else { continue };
            let Some(buttons) = msg.pointer("/Body/VirtualButtons").and_then(Value::as_array) else {
                continue;
            };
            let scenes: Vec<BorrowedSceneSnapshot> = buttons
                .iter()
                .filter(|b| b.get("IsProgrammed").and_then(Value::as_bool) == Some(true))
                .filter_map(|b| {
                    let title = b.get("Name").and_then(Value::as_str)?.trim();
                    let resource = b.get("href").and_then(Value::as_str)?;
                    (!title.is_empty()).then(|| BorrowedSceneSnapshot {
                        title: title.to_string(),
                        resource: resource.to_string(),
                        // No steps: LEAP does not say what a virtual button does, and a scene
                        // this driver cannot read is one core must not claim to know.
                        ..Default::default()
                    })
                })
                .collect();
            return vec![HostCall::BorrowedScenes { scenes }];
        }
        Vec::new()
    }

    /// An area's occupancy, as the bridge reports it for every group at once.
    fn on_occupancy_event(mine: &str, note: &str, args: &Args) -> Vec<HostCall> {
        if note != "rx" {
            return Vec::new();
        }
        let Some(text) = args.get("data").and_then(Value::as_str) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for line in text.split('\n').map(str::trim).filter(|l| !l.is_empty()) {
            let Ok(msg) = driver_sdk::serde_json::from_str::<Value>(line) else { continue };
            // One status, or the whole list — the bridge sends both shapes, a single push when
            // something changes and a multiple in reply to the read at bind.
            let one = msg.pointer("/Body/OccupancyGroupStatus").into_iter();
            let many = msg
                .pointer("/Body/OccupancyGroupStatuses")
                .and_then(Value::as_array)
                .into_iter()
                .flatten();
            for status in one.chain(many) {
                if status.pointer("/OccupancyGroup/href").and_then(Value::as_str) != Some(mine) {
                    continue; // another area, on the same bridge-wide feed
                }
                // `Unknown` is not a clear. A sensor that has not reported since the bridge
                // restarted has said nothing, and reporting that as "nobody is here" turns the
                // lights off in a room with somebody in it.
                let detected = match status.get("OccupancyStatus").and_then(Value::as_str) {
                    Some("Occupied") => true,
                    Some("Unoccupied") => false,
                    _ => continue,
                };
                let mut a = Args::new();
                a.insert("detected".into(), json!(detected));
                out.push(HostCall::notify(1, "detected_changed", a));
            }
        }
        out
    }

    /// `buttons` is this instance's own hrefs, from `pico_buttons` — the same list `on_bind`
    /// subscribed with, so a press on someone else's Pico read over the same connection is
    /// never mistaken for one of these.
    fn on_pico_event(inst: &mut Instance, note: &str, args: &Args) -> Vec<HostCall> {
        // The wake-up a press asked for. If it arrives, the key is still down — a release
        // would have cancelled nothing, but it clears the flag this sets, and the two cannot
        // both be true at once because they are the same key's one clock.
        if note == "timer" {
            let Some(key) = args
                .get("note")
                .and_then(Value::as_str)
                .and_then(|n| n.strip_prefix("held:"))
                .and_then(|n| n.parse::<u64>().ok())
            else {
                return Vec::new();
            };
            inst.scratch.insert(held_note(key), json!(true));
            let mut a = Args::new();
            a.insert("key".into(), json!(key));
            return vec![
                HostCall::notify(1, "held", a),
                HostCall::SetState {
                    proxy: 1,
                    key: "last_action".into(),
                    value: json!(format!("key {key} held")),
                },
            ];
        }
        if note != "rx" {
            return Vec::new();
        }
        let buttons = pico_buttons(inst);
        let mine = device_href(inst);
        let mine = mine.as_deref();
        let Some(text) = args.get("data").and_then(Value::as_str) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        for line in text.split('\n').map(str::trim).filter(|l| !l.is_empty()) {
            let Ok(msg) = driver_sdk::serde_json::from_str::<Value>(line) else { continue };

            // The remote's own status, which is where its battery is. One reply per remote on a
            // shared connection, so it has to be checked against ours the same way a press is:
            // a bridge with four Picos behind it answers for all of them down one socket.
            if let Some(status) = msg.pointer("/Body/DeviceStatus") {
                if status.pointer("/Device/href").and_then(Value::as_str) == mine
                    && let Some(percent) = status
                        .pointer("/BatteryStatus/LevelState")
                        .and_then(Value::as_str)
                        .and_then(battery_percent)
                {
                    let mut a = Args::new();
                    a.insert("percent".into(), json!(percent));
                    out.push(HostCall::notify(1, "battery_changed", a));
                }
                continue;
            }

            let Some(status) = msg.pointer("/Body/ButtonStatus") else { continue };
            let href = status.pointer("/Button/href").and_then(Value::as_str).unwrap_or("");
            let Some(key) = buttons.iter().find(|(_, h)| h == href).map(|(k, _)| *k) else {
                continue; // someone else's Pico, on the same event stream
            };
            // `clicked`, which is the only action every keypad has and the one every rule is
            // written against. This used to send `pressed` and `released`, and both were wrong:
            // the contract has no `pressed` at all, and `released` is gated behind `has_hold`
            // because it means "a long press ended" — its own doc says a plain click reports
            // `clicked` and nothing else. So core refused both, every press did nothing, and the
            // only sign was a line in the log saying a capability was not declared.
            //
            // On the press rather than the release. Lutron leaves click and hold timing to
            // whoever is listening, and this driver declares `has_hold = false` — it cannot tell
            // a hold from a click, so every press is a click and there is nothing to wait for.
            // Waiting for the release to be sure would only add the length of somebody's thumb
            // to every light in the house.
            // A press starts a clock and says nothing yet; what it turns out to be is decided
            // by which happens first, the wake-up or the release. That is the discrimination
            // Lutron expects of a listener and could not be done here until a driver could ask
            // to be woken — see `HOLD_MS` and `HostCall::After`.
            match status.pointer("/ButtonEvent/EventType").and_then(Value::as_str) {
                Some("Press") => {
                    // Restarts rather than stacks: one note per key, so a second press on the
                    // same key moves that key's clock instead of leaving one running.
                    out.push(HostCall::After { ms: HOLD_MS, note: held_note(key) });
                    continue;
                }
                Some("Release") => {}
                _ => continue, // a firmware shape this driver does not know yet
            }

            // Let go. If the wake-up already came, the hold was reported and this ends it;
            // otherwise it never crossed the line and was a click all along.
            let was_held = held_flag(inst, key);
            let name = if was_held { "released" } else { "clicked" };
            if was_held {
                inst.scratch.remove(&held_note(key));
            }
            let mut a = Args::new();
            a.insert("key".into(), json!(key));
            out.push(HostCall::notify(1, name, a));
            // Same reasoning as the dimmer's own tile: a keypad has nothing else to draw, and
            // one showing blank forever reads as broken rather than idle.
            out.push(HostCall::SetState {
                proxy: 1,
                key: "last_action".into(),
                value: json!(format!("key {key} {name}")),
            });
        }
        out
    }
}

export_driver!(CasetaLeap);

#[cfg(test)]
mod tests {
    use super::*;

    /// A Pico as it is once adopted: its own href, and the keys the bridge gave it.
    fn a_pico() -> Instance {
        let mut inst = Instance::default();
        inst.properties.insert("Device href".into(), json!("/device/6"));
        inst.properties.insert("Button 1 href".into(), json!("/button/9"));
        inst.properties.insert("Button 2 href".into(), json!("/button/10"));
        inst
    }

    /// The two pieces of Lutron knowledge that used to live in the controller.
    ///
    /// A bridge volunteers acknowledgements ahead of the reply you asked for, and announces a
    /// button press unprompted. Neither is something a generic transport could recognize —
    /// which is the whole reason this logic belongs to the driver.
    #[test]
    fn the_answer_is_picked_out_from_what_the_bridge_volunteered() {
        let stream = "\
{\"CommuniqueType\":\"SubscribeResponse\",\"Body\":{\"noise\":1}}
{\"CommuniqueType\":\"SubscribeResponse\",\"Body\":{\"noise\":2}}
{\"CommuniqueType\":\"ReadResponse\",\"Body\":{\"Devices\":[{\"Name\":\"Kitchen\"}]}}
";
        let answer = leap_answer(stream);
        assert_eq!(answer["CommuniqueType"], "ReadResponse");
        assert_eq!(answer["Body"]["Devices"][0]["Name"], "Kitchen");

        // Nothing but acknowledgements is not an answer, and must not be mistaken for one.
        assert!(leap_answer("{\"CommuniqueType\":\"SubscribeResponse\"}\n").is_null());
        // A half-written line is normal when a read window closes mid-message.
        assert!(leap_answer("{\"Communiqu").is_null());
    }

    /// The bug this pins: core's read returns as soon as the socket goes quiet for a moment,
    /// and a real bridge's first reply to `/device` is nothing but the acknowledgements it
    /// volunteers on connect. Concluding "no devices" from that listed none of the three
    /// behind a real bridge and told somebody there was nothing to add.
    #[test]
    fn an_answer_that_has_not_arrived_yet_is_read_again_rather_than_called_empty() {
        let state = json!({ "browse": true, "stage": "listing", "tries": "0" });
        let mut only_noise = Args::new();
        only_noise.insert("session".into(), json!(7));
        only_noise.insert(
            "received".into(),
            json!("{\"CommuniqueType\":\"SubscribeResponse\",\"Header\":{\"StatusCode\":\"204 NoContent\"}}\n"),
        );

        let (step, next) = CasetaLeap::handle_device_list(&state, &only_noise, false);
        match step {
            SetupStep::Session { session, open, send, .. } => {
                assert_eq!(session, Some(7), "the same connection, not a second handshake");
                assert!(open.is_none() && send.is_empty(), "listening again, not asking again");
            }
            other => panic!("expected another read, got {other:?}"),
        }
        assert_eq!(next["tries"], "1");

        // The bridge gets a bounded number of chances, and then this is a failure to report
        // rather than an empty list to draw.
        let spent = json!({ "browse": true, "stage": "listing", "tries": READ_RETRIES.to_string() });
        assert!(matches!(
            CasetaLeap::handle_device_list(&spent, &only_noise, false).0,
            SetupStep::Failed { .. }
        ));

        // And a connection that could not be opened is that failure, not an empty house.
        let mut refused = Args::new();
        refused.insert("error".into(), json!("connection refused"));
        assert!(matches!(
            CasetaLeap::handle_device_list(&state, &refused, false).0,
            SetupStep::Failed { .. }
        ));
    }

    #[test]
    fn a_button_press_is_recognized_only_from_the_bridges_own_push() {
        let pressed = "{\"Body\":{\"Status\":{\"Permissions\":[\"PhysicalAccess\"]}}}\n";
        assert!(CasetaLeap::button_pressed(pressed));

        // Connected but not yet pressed: the bridge chatters, and none of it means go.
        assert!(!CasetaLeap::button_pressed(
            "{\"Body\":{\"Status\":{\"Permissions\":[]}}}\n{\"CommuniqueType\":\"SubscribeResponse\"}\n"
        ));
        assert!(!CasetaLeap::button_pressed(""));
    }

    /// Every notification this driver can emit, held against the contract of the proxy it is
    /// emitted on — with that proxy's *adopted* capabilities, since half of them are gated.
    ///
    /// This is the check that was missing when `pressed` and `released` shipped. Core refuses an
    /// illegal notification at runtime and says so in the log, which is the right thing for it
    /// to do and the wrong place to find out: the house is installed, the button is wired, and
    /// the only symptom is that nothing happens.
    #[test]
    fn every_notification_this_driver_sends_is_one_its_contract_allows() {
        let registry = driver_sdk::proxy::ProxyRegistry::bundled().expect("bundled contracts");
        let claims: &[(&str, &[(&str, Value)], &[&str])] = &[
            ("keypad", &[("has_battery", json!(true)), ("has_hold", json!(true))],
                &["clicked", "held", "released", "battery_changed", "online_changed"]),
            ("light", &[("dimmer", json!(true)), ("supports_ramp", json!(true))],
                &["level_changed", "online_changed"]),
            ("switch", &[], &["switch_changed", "online_changed"]),
            ("fan", &[], &["speed_changed", "online_changed"]),
            ("blind", &[("supports_tilt", json!(true))],
                &["position_changed", "tilt_changed", "online_changed"]),
            ("sensor", &[("kind", json!("occupancy")), ("is_boolean", json!(true))],
                &["detected_changed", "online_changed"]),
        ];

        let no_args = BTreeMap::new();
        for (proxy_name, caps, sends) in claims {
            let proxy = registry
                .get(proxy_name)
                .unwrap_or_else(|| panic!("no `{proxy_name}` contract"));
            let declared: BTreeMap<String, Value> =
                caps.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect();
            let resolved = proxy
                .resolve(&declared)
                .unwrap_or_else(|e| panic!("`{proxy_name}` does not resolve: {e:?}"));
            for notification in *sends {
                // Names only: whether each carries the right arguments is the business of the
                // handler tests above, which drive real frames through and read what comes out.
                // Only the two failures that mean "this driver must not send this at all": the
                // name does not exist, or the capability gating it was never declared. A missing
                // argument is not one of those — whether each notification carries the right
                // ones is the business of the handler tests above, which drive real frames
                // through and read what comes out.
                match proxy.validate_notification(&resolved, notification, &no_args) {
                    Err(why @ driver_sdk::proxy::CallError::NoSuchCommand(_))
                    | Err(why @ driver_sdk::proxy::CallError::Unsupported { .. }) => {
                        panic!("`{proxy_name}` may not send `{notification}`: {why}")
                    }
                    _ => {}
                }
            }
        }

        // And the gate itself, still shut for a keypad that cannot tell a hold from a click.
        // `pressed` has never existed at all; `released` exists and is not for this device.
        let keypad = registry.get("keypad").expect("keypad contract");
        let quiet = keypad
            .resolve(&BTreeMap::from([("has_hold".to_string(), json!(false))]))
            .expect("a keypad with no hold is a keypad");
        for illegal in ["released", "pressed"] {
            assert!(
                keypad.validate_notification(&quiet, illegal, &no_args).is_err(),
                "`{illegal}` must not be sendable by a keypad that declares no hold",
            );
        }
    }

    /// Hold-to-dim, which is the pairing a keypad's `held`/`released` exists for: `ramp_start`
    /// fades toward an end of the travel, `ramp_stop` holds it where it got to.
    ///
    /// The bug this pins: `ramp_start` wrote its *destination* into state and reported it, so
    /// the house believed the lamp was at full the instant a thumb touched the button — and
    /// `ramp_stop` then "stopped" it by jumping to the level the driver had invented. Letting
    /// go early left the light exactly where the lie said it was.
    #[test]
    fn a_ramp_reports_nothing_and_stops_where_the_light_actually_got_to() {
        let mut inst = Instance::default();
        inst.properties.insert("Zone".into(), json!("/zone/3"));
        let sent = |calls: &[HostCall]| -> Value {
            let [HostCall::Tx { data, .. }] = calls else { panic!("expected one write, got {calls:?}") };
            driver_sdk::serde_json::from_slice(data).expect("valid JSON")
        };

        let up = {
            let mut a = Args::new();
            a.insert("direction".into(), json!("up"));
            a
        };
        let start = CasetaLeap::on_dimmer_command(&mut inst, "ramp_start", &up);
        let msg = sent(&start);
        assert_eq!(msg["Body"]["Command"]["DimmedLevelParameters"]["Level"], 100);
        assert_eq!(msg["Body"]["Command"]["DimmedLevelParameters"]["FadeTime"], "00:00:04");
        assert!(
            !start.iter().any(|c| matches!(c, HostCall::Notify { .. })),
            "a ramp has not arrived anywhere yet, so it reports nothing",
        );
        assert!(
            inst.scratch.get("level").is_none(),
            "and remembers nothing — the bridge is what says where the light is",
        );

        // The bridge reports the light passing 42 on its way up, and the thumb comes off.
        let mut heard = Args::new();
        heard.insert("data".into(), json!(
            "{\"Body\":{\"ZoneStatus\":{\"Zone\":{\"href\":\"/zone/3\"},\"Level\":42}}}\n"
        ));
        CasetaLeap::on_dimmer_event(&mut inst, "rx", &heard);
        let stop = sent(&CasetaLeap::on_dimmer_command(&mut inst, "ramp_stop", &Args::new()));
        assert_eq!(
            stop["Body"]["Command"]["DimmedLevelParameters"]["Level"], 42,
            "stopping holds it where it got to, not where it was headed",
        );
        assert_eq!(stop["Body"]["Command"]["DimmedLevelParameters"]["FadeTime"], "00:00:00");
    }

    #[test]
    fn a_message_is_framed_one_json_object_per_line() {
        let line = leap_line(&json!({ "Header": { "Url": "/device" } }));
        assert!(line.ends_with('\n'), "the bridge reads by line");
        assert_eq!(line.matches('\n').count(), 1);
        assert_eq!(leap_answer(&line)["Header"]["Url"], "/device");
    }

    #[test]
    fn every_pico_device_type_is_recognized_by_its_prefix() {
        assert!(is_pico("Pico2Button"));
        assert!(is_pico("Pico3ButtonRaiseLower"));
        assert!(is_pico("Pico4Button"));
        assert!(!is_pico("WallDimmer"));
        assert!(!is_pico("Dimmed"));
        assert!(!is_pico(""));
    }

    /// The bug this replaced: one manifest covers every Pico, so it declared the family's
    /// largest — five — and a two-button remote arrived with three keys that were drawn in the
    /// UI, offered in the automation editor, and connected to nothing.
    /// The bug this pins: the bridge files a two-key remote under a group of five buttons,
    /// numbered 0 to 4, and calls every one of them `Button N`. Counting what came back put
    /// five keys on a remote with two — drawn in the UI, offered to automations, impossible to
    /// press. Only the model says which numbers are real, and what they are called.
    #[test]
    fn a_pico_claims_the_keys_its_model_has_rather_than_every_number_the_bridge_listed() {
        let devices = vec![json!({
            "href": "/device/8",
            "Name": "Pico",
            "FullyQualifiedName": ["Kitchen", "Pico"],
            "DeviceType": "Pico2Button",
            "ButtonGroups": [{ "href": "/buttongroup/5" }],
        })];
        let buttons = (0..5)
            .map(|n| {
                json!({
                    "href": format!("/button/{}", 9 + n),
                    "ButtonNumber": n,
                    "Name": format!("Button {}", n + 1),
                    "Parent": { "href": "/buttongroup/5" },
                })
            })
            .collect::<Vec<_>>();

        let found = CasetaLeap::candidates(&json!({}), false, &devices, &buttons);
        let [pico] = found.as_slice() else { panic!("expected one candidate, got {found:?}") };

        assert_eq!(pico.label, "Pico", "the name, which is the leaf of the qualified one");
        assert_eq!(
            pico.room, "Kitchen",
            "and the area it was filed under, offered as the room to adopt it into",
        );
        assert_eq!(pico.capabilities["key_count"], json!(2));
        assert_eq!(pico.capabilities["key_labels"], json!("On,Off"));
        // On is button 0 and Off is button *2* — not the first two the bridge listed, which is
        // what makes this a table rather than a count.
        assert_eq!(pico.properties["Button 1 href"], json!("/button/9"));
        assert_eq!(pico.properties["Button 2 href"], json!("/button/11"));
        assert!(!pico.properties.contains_key("Button 3 href"));
    }

    /// A device the bridge files at the top of the project, and one filed two areas deep.
    #[test]
    fn the_room_offered_is_the_innermost_area_or_none_at_all() {
        let buttons = vec![json!({
            "href": "/button/1", "ButtonNumber": 0, "Parent": { "href": "/buttongroup/1" },
        })];
        let of = |fqn: Value| {
            let device = json!({
                "Name": "Pico",
                "FullyQualifiedName": fqn,
                "DeviceType": "Pico2Button",
                "ButtonGroups": [{ "href": "/buttongroup/1" }],
            });
            let found = CasetaLeap::candidates(&json!({}), false, &[device], &buttons);
            let [pico] = found.as_slice() else { panic!("expected one candidate") };
            (pico.label.clone(), pico.room.clone())
        };

        assert_eq!(of(json!(["Upstairs", "Kitchen", "Pico"])), ("Pico".into(), "Kitchen".into()));
        // Under no area: an answer, and not one to guess past by reaching for the name itself.
        assert_eq!(of(json!(["Pico"])), ("Pico".into(), String::new()));
    }

    /// The one model there is real hardware for here: a PJ-3BRL, five keys, whose buttons come
    /// back numbered 0 to 4 with no engraving on any of them.
    #[test]
    fn a_three_button_raise_lower_pico_is_named_the_way_lutron_engraves_it() {
        let devices = vec![json!({
            "href": "/device/6",
            "Name": "Pico",
            "FullyQualifiedName": ["Kitchen", "Pico"],
            "DeviceType": "Pico3ButtonRaiseLower",
            "ButtonGroups": [{ "href": "/buttongroup/5" }],
        })];
        let buttons = (0..5)
            .map(|n| {
                json!({
                    "href": format!("/button/{}", 116 + n),
                    "ButtonNumber": n,
                    "Parent": { "href": "/buttongroup/5" },
                })
            })
            .collect::<Vec<_>>();

        let found = CasetaLeap::candidates(&json!({}), false, &devices, &buttons);
        let [pico] = found.as_slice() else { panic!("expected one candidate, got {found:?}") };
        assert_eq!(pico.capabilities["key_count"], json!(5));
        assert_eq!(pico.capabilities["key_labels"], json!("On,Favorite,Off,Raise,Lower"));
        assert_eq!(pico.properties["Button 5 href"], json!("/button/120"));
    }

    #[test]
    fn a_picos_keys_are_its_own_and_a_model_nobody_wrote_down_still_gets_keys() {
        let device = json!({
            "DeviceType": "PicoSomethingUnheardOf",
            "ButtonGroups": [{ "href": "/buttongroup/8" }],
        });
        let buttons = vec![
            // Somebody else's remote, on the same bridge-wide list.
            json!({ "href": "/button/50", "ButtonNumber": 0, "Parent": { "href": "/buttongroup/9" } }),
            json!({ "href": "/button/10", "ButtonNumber": 1, "Parent": { "href": "/buttongroup/8" } }),
            json!({ "href": "/button/9", "ButtonNumber": 0, "Parent": { "href": "/buttongroup/8" } }),
        ];

        assert_eq!(
            pico_keys_of(&device, &buttons),
            vec![
                ("/button/9".to_string(), "Button 1".to_string()),
                ("/button/10".to_string(), "Button 2".to_string()),
            ],
            "an unknown model keeps every key the bridge listed, in ButtonNumber order",
        );

        // A device with no button group at all — an occupancy sensor, say — gets nothing to
        // subscribe to rather than a button that belongs to somebody else's remote.
        assert!(pico_keys_of(&json!({ "DeviceType": "Pico2Button" }), &buttons).is_empty());
    }

    /// A remote that has been heard from, and one whose battery is going. The bridge answers
    /// for every device behind it down one connection, so both have to be checked against ours.
    #[test]
    fn a_picos_battery_is_read_from_its_own_status_and_nobody_elses() {
        let mut inst = a_pico();
        let frame = |body: &str| {
            let mut args = Args::new();
            args.insert("data".into(), json!(body));
            args
        };

        let mine = concat!(
            "{\"Body\":{\"DeviceStatus\":{\"Device\":{\"href\":\"/device/6\"},",
            "\"BatteryStatus\":{\"LevelState\":\"Low\"}}}}\n"
        );
        let calls = CasetaLeap::on_pico_event(&mut inst, "rx", &frame(mine));
        match calls.as_slice() {
            [HostCall::Notify { name, args, .. }] => {
                assert_eq!(name, "battery_changed");
                assert_eq!(args["percent"], json!(10), "Low has to read as nearly empty");
            }
            other => panic!("expected one battery notification, got {other:?}"),
        }

        // The Pico in the next room, on the same socket.
        let theirs = concat!(
            "{\"Body\":{\"DeviceStatus\":{\"Device\":{\"href\":\"/device/9\"},",
            "\"BatteryStatus\":{\"LevelState\":\"Low\"}}}}\n"
        );
        assert!(CasetaLeap::on_pico_event(&mut inst, "rx", &frame(theirs)).is_empty());

        // A level this driver does not recognise says nothing rather than inventing a number.
        let odd = concat!(
            "{\"Body\":{\"DeviceStatus\":{\"Device\":{\"href\":\"/device/6\"},",
            "\"BatteryStatus\":{\"LevelState\":\"Flurgh\"}}}}\n"
        );
        assert!(CasetaLeap::on_pico_event(&mut inst, "rx", &frame(odd)).is_empty());
    }

    /// Every zone behind the bridge looks the same on the wire, so what tells them apart is the
    /// `DeviceType` table — transcribed from `pylutron-caseta`, because guessing which of forty
    /// names is a shade is how a driver silently refuses to adopt somebody's hardware.
    #[test]
    fn each_leap_device_type_is_adopted_as_the_thing_it_actually_is() {
        let device = |kind: &str| {
            json!({
                "href": "/device/2", "Name": "Thing", "FullyQualifiedName": ["Hall", "Thing"],
                "DeviceType": kind, "LocalZones": [{ "href": "/zone/3" }],
            })
        };
        let claimed = |kind: &str| {
            let found = CasetaLeap::candidates(&json!({}), false, &[device(kind)], &[]);
            let [c] = found.as_slice() else { panic!("{kind} was not adopted at all") };
            (c.driver_id.clone(), c.kind.clone(), c.properties["Kind"].clone())
        };

        assert_eq!(claimed("WallDimmer"), (DIMMER_ID.into(), "light".into(), json!("light")));
        assert_eq!(claimed("SunnataDimmer"), (DIMMER_ID.into(), "light".into(), json!("light")));
        assert_eq!(claimed("WallSwitch"), (SWITCH_ID.into(), "switch".into(), json!("switch")));
        assert_eq!(claimed("PowPakSwitch"), (SWITCH_ID.into(), "switch".into(), json!("switch")));
        assert_eq!(claimed("CasetaFanSpeedController"), (FAN_ID.into(), "fan".into(), json!("fan")));
        assert_eq!(claimed("SerenaRollerShade"), (SHADE_ID.into(), "blind".into(), json!("shade")));

        // Slats or not is the model's business. A roller shade must not offer a tilt control.
        let tilting = CasetaLeap::candidates(&json!({}), false, &[device("SerenaTiltOnlyWoodBlind")], &[]);
        assert_eq!(tilting[0].capabilities["supports_tilt"], json!(true));
        let rolling = CasetaLeap::candidates(&json!({}), false, &[device("SerenaRollerShade")], &[]);
        assert_eq!(rolling[0].capabilities["supports_tilt"], json!(false));

        // Something Lutron has not shipped yet is left alone rather than adopted as a guess.
        assert!(CasetaLeap::candidates(&json!({}), false, &[device("FluxCapacitor")], &[]).is_empty());
    }

    /// The fan's two translations: a speed this house names, and the speed Lutron calls it.
    #[test]
    fn a_fan_speaks_the_houses_speeds_and_lutrons_on_the_wire() {
        let mut inst = Instance::default();
        inst.properties.insert("Zone".into(), json!("/zone/3"));
        inst.properties.insert("Kind".into(), json!("fan"));

        let mut args = Args::new();
        args.insert("speed".into(), json!("medium_high"));
        let calls = CasetaLeap::on_fan_command(&mut inst, "set_speed", &args);
        let [HostCall::Tx { data, .. }] = calls.as_slice() else { panic!("expected one write") };
        let sent: Value = driver_sdk::serde_json::from_slice(data).expect("valid JSON");
        assert_eq!(sent["Body"]["Command"]["CommandType"], "GoToFanSpeed");
        assert_eq!(sent["Body"]["Command"]["FanSpeedParameters"]["FanSpeed"], "MediumHigh");

        // A speed this fan does not have is said out loud, not sent as itself.
        let mut nonsense = Args::new();
        nonsense.insert("speed".into(), json!("ludicrous"));
        assert!(matches!(
            CasetaLeap::on_fan_command(&mut inst, "set_speed", &nonsense).as_slice(),
            [HostCall::Log { .. }],
        ));

        // And back: the bridge's spelling becomes the house's, and `Off` is not a speed.
        let status = |speed: &str| {
            let mut a = Args::new();
            a.insert("data".into(), json!(format!(
                "{{\"Body\":{{\"ZoneStatus\":{{\"Zone\":{{\"href\":\"/zone/3\"}},\"FanSpeed\":\"{speed}\"}}}}}}\n"
            )));
            a
        };
        let calls = CasetaLeap::on_dimmer_event(&mut inst, "rx", &status("MediumHigh"));
        match calls.as_slice() {
            [HostCall::Notify { name, args, .. }] => {
                assert_eq!(name, "speed_changed");
                assert_eq!(args["speed"], json!("medium_high"));
                assert_eq!(args["on"], json!(true));
            }
            other => panic!("expected a speed report, got {other:?}"),
        }
        let calls = CasetaLeap::on_dimmer_event(&mut inst, "rx", &status("Off"));
        match calls.as_slice() {
            [HostCall::Notify { args, .. }] => {
                assert_eq!(args["on"], json!(false));
                assert_eq!(args["speed"], json!("off"));
            }
            other => panic!("expected a stop, got {other:?}"),
        }
        // Turning it back on returns it to the speed somebody chose, not to full.
        let on = CasetaLeap::on_fan_command(&mut inst, "on", &Args::new());
        let [HostCall::Tx { data, .. }] = on.as_slice() else { panic!("expected one write") };
        let sent: Value = driver_sdk::serde_json::from_slice(data).expect("valid JSON");
        assert_eq!(sent["Body"]["Command"]["FanSpeedParameters"]["FanSpeed"], "MediumHigh");
    }

    /// A shade's ends are levels and its verbs are the motor's. `GoToDimmedLevel` is a lighting
    /// command and a motor does not take one.
    #[test]
    fn a_shade_is_driven_by_level_and_stopped_by_its_own_verb() {
        let mut inst = Instance::default();
        inst.properties.insert("Zone".into(), json!("/zone/7"));
        inst.properties.insert("Kind".into(), json!("shade"));
        let sent = |inst: &mut Instance, cmd: &str, args: &Args| {
            let calls = CasetaLeap::on_shade_command(inst, cmd, args);
            let [HostCall::Tx { data, .. }] = calls.as_slice() else { panic!("expected one write") };
            driver_sdk::serde_json::from_slice::<Value>(data).expect("valid JSON")
        };

        let open = sent(&mut inst, "open", &Args::new());
        assert_eq!(open["Body"]["Command"]["CommandType"], "GoToLevel");
        assert_eq!(open["Body"]["Command"]["Parameter"][0]["Value"], 100);
        assert_eq!(sent(&mut inst, "close", &Args::new())["Body"]["Command"]["Parameter"][0]["Value"], 0);
        assert_eq!(sent(&mut inst, "stop", &Args::new())["Body"]["Command"]["CommandType"], "Stop");

        let mut tilt = Args::new();
        tilt.insert("tilt".into(), json!(40));
        let tilted = sent(&mut inst, "set_tilt", &tilt);
        assert_eq!(tilted["Body"]["Command"]["CommandType"], "GoToTilt");
        assert_eq!(tilted["Body"]["Command"]["TiltParameters"]["Tilt"], 40);

        // And its status is a position, not a brightness.
        let mut a = Args::new();
        a.insert("data".into(), json!(
            "{\"Body\":{\"ZoneStatus\":{\"Zone\":{\"href\":\"/zone/7\"},\"Level\":60}}}\n"
        ));
        match CasetaLeap::on_dimmer_event(&mut inst, "rx", &a).as_slice() {
            [HostCall::Notify { name, args, .. }] => {
                assert_eq!(name, "position_changed");
                assert_eq!(args["position"], json!(60));
            }
            other => panic!("expected a position, got {other:?}"),
        }
    }

    /// A switched load has no fade and no levels in between.
    #[test]
    fn a_switch_is_sent_a_plain_level_and_toggles_from_what_it_last_reported() {
        let mut inst = Instance::default();
        inst.properties.insert("Zone".into(), json!("/zone/9"));
        inst.properties.insert("Kind".into(), json!("switch"));

        let calls = CasetaLeap::on_switch_command(&mut inst, "on");
        let [HostCall::Tx { data, .. }] = calls.as_slice() else { panic!("expected one write") };
        let sent: Value = driver_sdk::serde_json::from_slice(data).expect("valid JSON");
        assert_eq!(sent["Body"]["Command"]["CommandType"], "GoToLevel", "no fade on a switch");
        assert_eq!(sent["Body"]["Command"]["Parameter"][0]["Value"], 100);

        // The bridge says it is on; a toggle then has to turn it off rather than on again.
        let mut a = Args::new();
        a.insert("data".into(), json!(
            "{\"Body\":{\"ZoneStatus\":{\"Zone\":{\"href\":\"/zone/9\"},\"Level\":100}}}\n"
        ));
        match CasetaLeap::on_dimmer_event(&mut inst, "rx", &a).as_slice() {
            [HostCall::Notify { name, args, .. }] => {
                assert_eq!(name, "switch_changed");
                assert_eq!(args["on"], json!(true));
            }
            other => panic!("expected a switch report, got {other:?}"),
        }
        let calls = CasetaLeap::on_switch_command(&mut inst, "toggle");
        let [HostCall::Tx { data, .. }] = calls.as_slice() else { panic!("expected one write") };
        let sent: Value = driver_sdk::serde_json::from_slice(data).expect("valid JSON");
        assert_eq!(sent["Body"]["Command"]["Parameter"][0]["Value"], 0, "on, so a toggle is off");
    }

    /// The bug this pins: every Caséta bridge has an occupancy group per area whether or not
    /// anybody put a sensor in it — this one has three and no sensors at all. Offering those
    /// would put a motion sensor in the house for every room the installer ever named.
    #[test]
    fn only_an_area_with_a_sensor_in_it_is_offered_as_one() {
        let areas = vec![
            json!({ "href": "/area/2", "Name": "Master Bedroom" }),
            json!({ "href": "/area/4", "Name": "Kitchen" }),
        ];
        let groups = vec![
            // A slot with nothing in it — what a real bridge is full of.
            json!({ "href": "/occupancygroup/1", "AssociatedAreas": [{ "Area": { "href": "/area/2" } }] }),
            json!({
                "href": "/occupancygroup/3",
                "AssociatedAreas": [{ "Area": { "href": "/area/4" } }],
                "AssociatedSensors": [{ "OccupancySensor": { "href": "/device/8" } }],
            }),
        ];

        let found = occupancy_candidates(&areas, &groups);
        let [sensor] = found.as_slice() else { panic!("expected one, got {found:?}") };
        assert_eq!(sensor.label, "Kitchen Occupancy");
        assert_eq!(sensor.room, "Kitchen", "it goes in the room it watches");
        assert_eq!(sensor.driver_id, OCCUPANCY_ID);
        assert_eq!(sensor.properties["Occupancy href"], json!("/occupancygroup/3"));
    }

    /// One feed carries every area on the bridge, and `Unknown` is not a clear.
    #[test]
    fn occupancy_is_filtered_to_this_area_and_unknown_says_nothing() {
        let frame = |body: &str| {
            let mut a = Args::new();
            a.insert("data".into(), json!(body));
            a
        };
        let status = |group: &str, state: &str| {
            format!(
                "{{\"Body\":{{\"OccupancyGroupStatus\":{{\"OccupancyGroup\":{{\"href\":\"{group}\"}},\
                 \"OccupancyStatus\":\"{state}\"}}}}}}\n"
            )
        };

        let calls = CasetaLeap::on_occupancy_event("/occupancygroup/3", "rx", &frame(&status("/occupancygroup/3", "Occupied")));
        match calls.as_slice() {
            [HostCall::Notify { name, args, .. }] => {
                assert_eq!(name, "detected_changed");
                assert_eq!(args["detected"], json!(true));
            }
            other => panic!("expected a detection, got {other:?}"),
        }

        // The room next door, on the same bridge-wide feed.
        assert!(CasetaLeap::on_occupancy_event(
            "/occupancygroup/3", "rx", &frame(&status("/occupancygroup/9", "Occupied")),
        ).is_empty());

        // A sensor that has said nothing since the bridge restarted has said nothing. Reporting
        // that as "nobody is here" turns the lights off in a room with somebody in it.
        assert!(CasetaLeap::on_occupancy_event(
            "/occupancygroup/3", "rx", &frame(&status("/occupancygroup/3", "Unknown")),
        ).is_empty());

        // And the whole-list shape, which is what the read at bind answers with.
        let many = concat!(
            "{\"Body\":{\"OccupancyGroupStatuses\":[",
            "{\"OccupancyGroup\":{\"href\":\"/occupancygroup/1\"},\"OccupancyStatus\":\"Occupied\"},",
            "{\"OccupancyGroup\":{\"href\":\"/occupancygroup/3\"},\"OccupancyStatus\":\"Unoccupied\"}]}}\n"
        );
        match CasetaLeap::on_occupancy_event("/occupancygroup/3", "rx", &frame(many)).as_slice() {
            [HostCall::Notify { args, .. }] => assert_eq!(args["detected"], json!(false)),
            other => panic!("expected one clear for this area only, got {other:?}"),
        }
    }

    /// A virtual button nobody programmed is a switch wired to nothing.
    #[test]
    fn only_a_programmed_virtual_button_is_offered_as_a_scene() {
        let mut a = Args::new();
        a.insert("data".into(), json!(concat!(
            "{\"Body\":{\"VirtualButtons\":[",
            "{\"href\":\"/virtualbutton/1\",\"Name\":\"Arriving Home\",\"IsProgrammed\":true},",
            "{\"href\":\"/virtualbutton/3\",\"Name\":\"Button 3\",\"IsProgrammed\":false}]}}\n"
        )));
        match CasetaLeap::on_bridge_event("rx", &a).as_slice() {
            [HostCall::BorrowedScenes { scenes }] => {
                assert_eq!(scenes.len(), 1, "the unprogrammed one is not a scene");
                assert_eq!(scenes[0].title, "Arriving Home");
                assert_eq!(scenes[0].resource, "/virtualbutton/1");
                assert!(scenes[0].steps.is_empty(), "LEAP does not say what it does");
            }
            other => panic!("expected one scene import, got {other:?}"),
        }
    }

    /// What a press turns out to be, decided by which happens first: the wake-up or the release.
    ///
    /// The bug this replaced: the driver sent `pressed` and `released`, and core refused both —
    /// the keypad contract has no `pressed`, and `released` means "a long press ended" and is
    /// gated behind `has_hold`. Every press did nothing, and the only sign was a line in the log
    /// about an undeclared capability.
    #[test]
    fn a_short_press_is_a_click_and_a_long_one_is_a_hold_then_a_release() {
        let mut inst = a_pico();
        let frame = |body: String| {
            let mut a = Args::new();
            a.insert("data".into(), json!(body));
            a
        };
        let event = |href: &str, what: &str| format!(
            "{{\"Body\":{{\"ButtonStatus\":{{\"Button\":{{\"href\":\"{href}\"}},\"ButtonEvent\":{{\"EventType\":\"{what}\"}}}}}}}}\n"
        );
        let named = |calls: &[HostCall]| -> Vec<String> {
            calls.iter().filter_map(|c| match c {
                HostCall::Notify { name, .. } => Some(name.clone()),
                _ => None,
            }).collect()
        };

        // A press says nothing yet: it starts a clock for that key and waits.
        let started = CasetaLeap::on_pico_event(&mut inst, "rx", &frame(event("/button/10", "Press")));
        assert!(named(&started).is_empty(), "a press is not yet a click");
        match started.as_slice() {
            [HostCall::After { ms, note }] => {
                assert_eq!(*ms, HOLD_MS);
                assert_eq!(note, "held:2", "one clock, named for the key it belongs to");
            }
            other => panic!("expected a wake-up to be asked for, got {other:?}"),
        }

        // Let go before it goes off: a click, and no hold was ever reported.
        let quick = CasetaLeap::on_pico_event(&mut inst, "rx", &frame(event("/button/10", "Release")));
        assert_eq!(named(&quick), vec!["clicked"]);

        // Hold it instead. The wake-up arrives first, so the hold is reported...
        CasetaLeap::on_pico_event(&mut inst, "rx", &frame(event("/button/10", "Press")));
        let mut woken = Args::new();
        woken.insert("note".into(), json!("held:2"));
        assert_eq!(named(&CasetaLeap::on_pico_event(&mut inst, "timer", &woken)), vec!["held"]);

        // ...and letting go ends it rather than counting as a click, which is what makes
        // `held` → ramp and `released` → stop a pair rather than two unrelated rules.
        let ended = CasetaLeap::on_pico_event(&mut inst, "rx", &frame(event("/button/10", "Release")));
        assert_eq!(named(&ended), vec!["released"]);

        // And the next press is a fresh question, not a hold left over.
        CasetaLeap::on_pico_event(&mut inst, "rx", &frame(event("/button/10", "Press")));
        let again = CasetaLeap::on_pico_event(&mut inst, "rx", &frame(event("/button/10", "Release")));
        assert_eq!(named(&again), vec!["clicked"], "the hold flag has to be cleared");

        // Somebody else's Pico on the same connection is still ignored.
        assert!(CasetaLeap::on_pico_event(&mut inst, "rx", &frame(event("/button/99", "Press"))).is_empty());
        assert!(CasetaLeap::on_pico_event(&mut inst, "not-rx", &Args::new()).is_empty());
    }
}
