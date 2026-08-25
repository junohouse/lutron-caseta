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
//!   `/device` lists what is paired, `/buttongroup` lists a Pico's buttons; a
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
/// `pico_keys`, answered per device at adoption.
const MAX_BUTTONS: usize = 5;

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
    /// more read first. A button lives in `/buttongroup`, a separate collection `/device`
    /// only points at, so a Pico cannot be offered from this reply alone the way a dimmer can.
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
            return Self::request_button_groups(state, input, include_bridge, devices);
        }
        (SetupStep::done(Self::candidates(state, include_bridge, &devices, &[])), Value::Null)
    }

    /// One more read, made only when something found in `/device` needs it — most bridges have
    /// no Pico behind them and never pay for this round trip.
    fn request_button_groups(
        state: &Value,
        input: &Args,
        include_bridge: bool,
        devices: Vec<Value>,
    ) -> (SetupStep, Value) {
        let body = json!({ "CommuniqueType": "ReadRequest", "Header": { "Url": "/buttongroup" } });

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

    fn handle_button_groups(state: &Value, input: &Args, include_bridge: bool) -> (SetupStep, Value) {
        if let Some(err) = input.get("error").and_then(Value::as_str) {
            return (
                SetupStep::Failed { reason: format!("could not read the bridge's buttons: {err}") },
                Value::Null,
            );
        }
        let response = leap_answer(input.get("received").and_then(Value::as_str).unwrap_or(""));
        let Some(groups) = response.pointer("/Body/ButtonGroups").and_then(Value::as_array).cloned()
        else {
            return Self::read_again(state, input, "listing_buttons").unwrap_or((
                SetupStep::Failed { reason: "the bridge did not answer with its button groups".into() },
                Value::Null,
            ));
        };
        let devices: Vec<Value> = state.get("devices_json").and_then(Value::as_array).cloned().unwrap_or_default();
        (SetupStep::done(Self::candidates(state, include_bridge, &devices, &groups)), Value::Null)
    }

    fn candidates(state: &Value, include_bridge: bool, devices: &[Value], button_groups: &[Value]) -> Vec<Candidate> {
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

        // ponytail: dimmers and Picos only. Switches, RA2-style wired keypads, and occupancy
        // sensors are real LEAP device types too — add a DeviceType arm here plus a manifest
        // for each when one is needed.
        const DIMMABLE: &[&str] = &["WallDimmer", "PlugInDimmer", "InLineDimmer", "Dimmed"];
        for d in devices {
            let kind = d.get("DeviceType").and_then(Value::as_str).unwrap_or("");
            let name = caseta_name(d);

            if DIMMABLE.contains(&kind) {
                let Some(zone) = d.pointer("/LocalZones/0/href").and_then(Value::as_str) else {
                    continue; // no zone means nothing to command
                };
                let mut props = BTreeMap::new();
                props.insert("Zone".into(), json!(zone));
                out.push(Candidate {
                    label: name,
                    kind: "light".into(),
                    driver_id: DIMMER_ID.into(),
                    properties: props,
                    verified: "found on bridge".into(),
                    ..Default::default()
                });
                continue;
            }

            if is_pico(kind) {
                let Some(href) = d.get("href").and_then(Value::as_str) else { continue };
                let buttons = pico_button_hrefs(href, button_groups);
                if buttons.is_empty() {
                    continue; // no buttons found for it means nothing to subscribe to
                }
                let mut props = BTreeMap::new();
                for (i, b) in buttons.iter().take(MAX_BUTTONS).enumerate() {
                    props.insert(format!("Button {} href", i + 1), json!(b));
                }
                out.push(Candidate {
                    label: name,
                    kind: "keypad".into(),
                    driver_id: PICO_ID.into(),
                    properties: props,
                    capabilities: pico_keys(buttons.len().min(MAX_BUTTONS)),
                    verified: "found on bridge".into(),
                    ..Default::default()
                });
            }
        }

        out
    }
}

/// What the Caséta app calls this device.
///
/// `Name` alone is the leaf — a Pico in the kitchen is called `Pico`, and so is the one in the
/// hall. `FullyQualifiedName` is the same name with the area in front of it, which is what the
/// app shows and what somebody adopting one recognises: `Kitchen Pico`. Falls back to `Name`
/// for anything the bridge files outside an area, such as the bridge itself.
fn caseta_name(device: &Value) -> String {
    let parts: Vec<&str> = device
        .get("FullyQualifiedName")
        .and_then(Value::as_array)
        .map(|parts| parts.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !parts.is_empty() {
        return parts.join(" ");
    }
    device
        .get("Name")
        .and_then(Value::as_str)
        .unwrap_or("Caséta device")
        .to_string()
}

/// Any Pico remote — `Pico2Button`, `Pico3ButtonRaiseLower`, `Pico4Button` and the rest all
/// start with it, and this driver treats every one of them the same way (see `PICO_ID`'s
/// manifest for the ceiling that puts a real name on).
fn is_pico(device_type: &str) -> bool {
    device_type.starts_with("Pico")
}

/// How many keys *this* Pico has, for the candidate that is about to be adopted.
///
/// The manifest cannot answer it. "Pico" is a family, not a product: a Pico2Button has two keys
/// and a Pico3ButtonRaiseLower has five, and one manifest covers both because they differ in
/// nothing else. Declaring the largest meant a two-button remote arrived with three keys that
/// were drawn in the UI, offered in the automation editor and impossible to press — the same
/// mistake a four-HDMI declaration makes on a three-port television, and core has the same
/// answer for it: the driver knows, so the driver says. See `Candidate::capabilities`.
///
/// ponytail: numbered labels, because the bridge's ButtonGroups carry hrefs and no names. The
/// DeviceType does imply Lutron's engraving — On/Favorite/Off/Raise/Lower on a
/// Pico3ButtonRaiseLower — but the mapping from that to the order the hrefs arrive in is not
/// something this has been checked against real hardware for, and a key labelled `Off` that
/// turns the lights on is worse than one labelled `Button 4`. Read `/button/<id>` for the real
/// engraving if this is worth another round trip.
fn pico_keys(count: usize) -> BTreeMap<String, Value> {
    let labels = (1..=count).map(|n| format!("Button {n}")).collect::<Vec<_>>();
    BTreeMap::from([
        ("key_count".to_string(), json!(count)),
        ("key_labels".to_string(), json!(labels.join(","))),
    ])
}

/// This Pico's button hrefs, in the order the bridge lists them — physical order, since Lutron
/// assigns them at commissioning top to bottom.
fn pico_button_hrefs<'a>(device_href: &str, button_groups: &'a [Value]) -> Vec<&'a str> {
    button_groups
        .iter()
        .filter(|g| g.pointer("/Parent/href").and_then(Value::as_str) == Some(device_href))
        .flat_map(|g| g.pointer("/Buttons").and_then(Value::as_array).into_iter().flatten())
        .filter_map(|b| b.get("href").and_then(Value::as_str))
        .collect()
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
                "listing_buttons" => Self::handle_button_groups(state, input, false),
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
            "listing_buttons" => Self::handle_button_groups(state, input, true),
            _ => Self::ask_address(state, input),
        }
    }

    fn on_command(&self, inst: &mut Instance, _proxy: LocalId, cmd: &str, args: &Args) -> Vec<HostCall> {
        // The bridge and a Pico's keypad proxy both take no commands (see their manifests) —
        // anything reaching here is for the dimmer's light proxy.
        Self::on_dimmer_command(inst, cmd, args)
    }

    fn on_event(&self, inst: &mut Instance, _control: LocalId, note: &str, args: &Args) -> Vec<HostCall> {
        let buttons = pico_buttons(inst);
        if !buttons.is_empty() {
            return Self::on_pico_event(&buttons, note, args);
        }
        Self::on_dimmer_event(inst, note, args)
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
        for (_, href) in pico_buttons(inst) {
            out.push(tx(&subscribe_button(&href)));
        }
        out
    }
}

// ---------------------------------------------------------------------------------------
// The live connection — one zone's on/off/dim, and the status push that keeps it in sync.
// ---------------------------------------------------------------------------------------

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
            "ramp_start" => {
                let up = args.get("direction").and_then(Value::as_str) == Some("up");
                (if up { 100 } else { 1 }, 4)
            }
            "ramp_stop" => (last, 0),
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

    /// `buttons` is this instance's own hrefs, from `pico_buttons` — the same list `on_bind`
    /// subscribed with, so a press on someone else's Pico read over the same connection is
    /// never mistaken for one of these.
    fn on_pico_event(buttons: &[(u64, String)], note: &str, args: &Args) -> Vec<HostCall> {
        if note != "rx" {
            return Vec::new();
        }
        let Some(text) = args.get("data").and_then(Value::as_str) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        for line in text.split('\n').map(str::trim).filter(|l| !l.is_empty()) {
            let Ok(msg) = driver_sdk::serde_json::from_str::<Value>(line) else { continue };
            let Some(status) = msg.pointer("/Body/ButtonStatus") else { continue };
            let href = status.pointer("/Button/href").and_then(Value::as_str).unwrap_or("");
            let Some(key) = buttons.iter().find(|(_, h)| h == href).map(|(k, _)| *k) else {
                continue; // someone else's Pico, on the same event stream
            };
            // Only Press/Release is real on Caséta's own bridge — Lutron leaves click/hold
            // timing to whoever is listening rather than doing it on-device, which is why
            // this manifest declares neither `has_hold` nor `has_double`.
            let name = match status.pointer("/ButtonEvent/EventType").and_then(Value::as_str) {
                Some("Press") => "pressed",
                Some("Release") => "released",
                _ => continue, // a firmware shape this driver does not know yet
            };
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
    #[test]
    fn a_pico_claims_the_keys_it_has_rather_than_the_manifests_five() {
        let devices = vec![json!({
            "href": "/device/8",
            "Name": "Kitchen Pico",
            "DeviceType": "Pico2Button",
        })];
        let groups = vec![json!({
            "Parent": { "href": "/device/8" },
            "Buttons": [{ "href": "/button/9" }, { "href": "/button/10" }],
        })];
        let found = CasetaLeap::candidates(&json!({}), false, &devices, &groups);
        let [pico] = found.as_slice() else { panic!("expected one candidate, got {found:?}") };

        assert_eq!(pico.capabilities["key_count"], json!(2));
        assert_eq!(pico.capabilities["key_labels"], json!("Button 1,Button 2"));
        // And the hrefs stop at two as well — a third property would be read back by
        // `pico_buttons` as a key to subscribe to.
        assert_eq!(pico.properties["Button 1 href"], json!("/button/9"));
        assert_eq!(pico.properties["Button 2 href"], json!("/button/10"));
        assert!(!pico.properties.contains_key("Button 3 href"));
    }

    #[test]
    fn a_picos_buttons_come_from_the_group_that_belongs_to_it_in_bridge_order() {
        let groups = vec![
            json!({
                "Parent": { "href": "/device/9" },
                "Buttons": [{ "href": "/button/50" }, { "href": "/button/51" }],
            }),
            json!({
                "Parent": { "href": "/device/8" },
                "Buttons": [
                    { "href": "/button/9" },
                    { "href": "/button/10" },
                    { "href": "/button/11" },
                ],
            }),
        ];
        assert_eq!(
            pico_button_hrefs("/device/8", &groups),
            vec!["/button/9", "/button/10", "/button/11"]
        );
        // A device with no button group at all — an occupancy sensor, say — gets nothing to
        // subscribe to rather than a button that belongs to somebody else's remote.
        assert!(pico_button_hrefs("/device/404", &groups).is_empty());
    }

    #[test]
    fn a_pico_event_reports_only_its_own_button_and_only_press_or_release() {
        let buttons = vec![(1u64, "/button/9".to_string()), (2u64, "/button/10".to_string())];
        let mut args = Args::new();
        args.insert(
            "data".into(),
            json!(concat!(
                // Somebody else's Pico on the same event stream — must be ignored.
                "{\"Body\":{\"ButtonStatus\":{\"Button\":{\"href\":\"/button/99\"},",
                "\"ButtonEvent\":{\"EventType\":\"Press\"}}}}\n",
                "{\"Body\":{\"ButtonStatus\":{\"Button\":{\"href\":\"/button/10\"},",
                "\"ButtonEvent\":{\"EventType\":\"Press\"}}}}\n",
                "{\"Body\":{\"ButtonStatus\":{\"Button\":{\"href\":\"/button/10\"},",
                "\"ButtonEvent\":{\"EventType\":\"Release\"}}}}\n",
            )),
        );
        let calls = CasetaLeap::on_pico_event(&buttons, "rx", &args);

        let notify_names: Vec<&str> = calls
            .iter()
            .filter_map(|c| match c {
                HostCall::Notify { name, args, .. } if args.get("key") == Some(&json!(2)) => {
                    Some(name.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(notify_names, vec!["pressed", "released"]);

        // An event that is neither Press nor Release, and one whose note is not `rx` at all:
        // both produce nothing rather than a guess.
        assert!(CasetaLeap::on_pico_event(&buttons, "not-rx", &args).is_empty());
    }
}
