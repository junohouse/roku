//! Roku players and Roku TVs over ECP (External Control Protocol), HTTP on port 8060.
//!
//! ```text
//!   POST /keypress/<key>        Home Up Down Left Right Select Back Play Pause Rev Fwd
//!                               VolumeUp VolumeDown VolumeMute PowerOn PowerOff Info Search
//!   POST /keypress/Lit_<char>   type one character
//!   POST /launch/<appId>        open a channel, optionally ?contentId=..&mediaType=..
//!   POST /launch/tvinput.hdmi1  Roku TV: switch to an HDMI input
//!   GET  /query/apps            installed channels, as XML
//!   GET  /query/active-app      what is in the foreground
//! ```
//!
//! # "Watch Netflix"
//!
//! The whole point of the app list. A Roku's channels are not knowable when the `media_player`
//! proxy is written, so the driver reads them from the device and reports them as state; the
//! proxy marks `launch_app.app` as `values_from = "apps"`, and the assistant is shown exactly
//! the channels this box has. Nobody hard-codes a list of streaming services anywhere.

use driver_sdk::*;
use driver_sdk::Value;

#[derive(Default)]
pub struct Roku;

/// Roku's well-known channel ids.
///
/// A device set to Limited control refuses `/query/apps` but still honours `/launch`, so
/// without this the channel list is empty and nothing can be launched at all — while the
/// hardware would happily have done it. These are Roku's own long-stable ids; anything not
/// installed simply does nothing when launched, which is why they are marked as unconfirmed
/// rather than presented as the real list.
///
/// This lives in the driver, not the controller: knowing what Roku calls Netflix is exactly
/// the vendor knowledge a driver exists to hold.
const WELL_KNOWN: &[(&str, &str)] = &[
    ("12", "Netflix"),
    ("837", "YouTube"),
    ("13", "Prime Video"),
    ("291097", "Disney+"),
    ("61322", "Max"),
    ("151908", "The Roku Channel"),
    ("2285", "Hulu"),
];

/// What to change, and where. Roku buries this three menus deep and the wording differs by
/// model, so name both the old and the new label rather than sending someone hunting.
const LIMITED_MODE: &str =
    "This Roku is set to Limited control, so it will not list or launch channels. On the \
     device: Settings → System → Advanced system settings → Control by mobile apps → \
     Network access, and choose Default or Permissive. Older models call it \
     Settings → System → Screen mirroring → Control by mobile apps.";

const MEDIA: LocalId = 1;

/// The screen, when this Roku has one.
///
/// A node rather than a second proxy on the same device: whether there is a screen is a fact
/// about the unit, not about the product, and `[[proxy]]` has to be answered in the manifest.
/// A Roku TV therefore adopts as a streamer with a `tv` child; a Roku Express adopts as the
/// streamer alone and grows nothing.
const SCREEN: &str = "screen";

/// Every presented node's own binding is its first, whatever contract it satisfies.
const NODE: LocalId = 1;

/// Something the screen has to say, aimed at the screen.
fn for_screen(note: &str, args: Args) -> HostCall {
    HostCall::ForNode {
        node: SCREEN.to_string(),
        calls: vec![HostCall::notify(NODE, note, args)],
    }
}

/// A connection id for one of a Roku TV's physical inputs, from the channel id it lists it
/// under — `tvinput.hdmi2` is 1002, matching the manifest's own numbering.
///
/// From the name rather than from list order, because a project remembers what an installer
/// wired by this number and the channel list is not ordered by anything stable. `None` for
/// every ordinary channel: Netflix is not a jack.
fn connection_id(app_id: &str) -> Option<LocalId> {
    let input = app_id.strip_prefix("tvinput.")?;
    if let Some(n) = input.strip_prefix("hdmi") {
        return n.parse::<LocalId>().ok().filter(|n| (1..=99).contains(n)).map(|n| 1000 + n);
    }
    match input {
        "cvbs" | "av" => Some(1101),
        "component" => Some(1102),
        "tuner" | "dtv" => Some(1201),
        _ => None,
    }
}

/// What kind of cable an input takes, for the pathfinder's own vocabulary.
fn signal_class(app_id: &str) -> &'static str {
    match connection_id(app_id) {
        Some(1101) => "COMPOSITE",
        Some(1102) => "COMPONENT",
        Some(1201) => "RF_UHF_VHF",
        _ => "HDMI",
    }
}

/// The channel id a connection is switched to — the inverse of [`connection_id`].
fn tvinput_for(connection: u64) -> Option<String> {
    match connection {
        1001..=1099 => Some(format!("tvinput.hdmi{}", connection - 1000)),
        1101 => Some("tvinput.cvbs".into()),
        1102 => Some("tvinput.component".into()),
        1201 => Some("tvinput.tuner".into()),
        _ => None,
    }
}

impl Roku {
    fn base(inst: &Instance) -> Option<String> {
        let addr = inst.property("Address").as_str()?.trim().to_string();
        if addr.is_empty() {
            return None;
        }
        let port = inst.property("Port").as_u64().unwrap_or(8060);
        Some(format!("http://{addr}:{port}"))
    }

    fn keypress(inst: &Instance, key: &str) -> Option<HostCall> {
        Some(HostCall::Http(HttpRequest::new(
            "POST",
            format!("{}/keypress/{key}", Self::base(inst)?),
        )))
    }

    fn get(inst: &Instance, path: &str) -> Option<HostCall> {
        Some(HostCall::Http(HttpRequest::new(
            "GET",
            format!("{}{path}", Self::base(inst)?),
        )))
    }

    /// Installed channels, from `id` to display name, cached on the instance at bind time.
    fn app_id(inst: &Instance, want: &str) -> Option<String> {
        let apps = inst.scratch.get("app_map")?.as_object()?;
        let want_norm = normalize(want);

        // Exact, then prefix, then substring: "netflix" -> "Netflix", "disney" -> "Disney+",
        // "prime" -> "Prime Video". People do not say the full channel name.
        let mut best: Option<(u8, &String)> = None;
        for (id, name) in apps {
            let n = normalize(name.as_str().unwrap_or(""));
            let rank = if n == want_norm {
                0
            } else if n.starts_with(&want_norm) || want_norm.starts_with(&n) {
                1
            } else if n.contains(&want_norm) || want_norm.contains(&n) {
                2
            } else {
                continue;
            };
            if best.is_none_or(|(r, _)| rank < r) {
                best = Some((rank, id));
            }
        }
        best.map(|(_, id)| id.clone())
    }
}

fn normalize(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// XML text back to the characters a person would say.
///
/// The names come out of an ECP document, so a channel called "ABC News: Live & Breaking
/// News" arrives as `Live &amp; Breaking News`. Left escaped it becomes an allowed value on
/// `launch_app`, and the assistant has to spell out `&amp;` to launch it — which it will
/// not, so the channel is simply unreachable by voice.
fn unescape(s: &str) -> String {
    // `&amp;` last, or `&amp;lt;` would turn into `<`.
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// Pull `<app id="12" ...>Netflix</app>` entries out of an ECP response.
///
/// A dependency-free scan rather than an XML parser: the document is a flat list of one
/// element type from a device on the LAN, and adding an XML crate to read it would be silly.
pub fn parse_apps(xml: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for chunk in xml.split("<app ").skip(1) {
        let Some(id_start) = chunk.find("id=\"") else {
            continue;
        };
        let rest = &chunk[id_start + 4..];
        let Some(id_end) = rest.find('"') else { continue };
        let id = &rest[..id_end];

        let Some(open) = chunk.find('>') else { continue };
        let Some(close) = chunk.find("</app>") else {
            continue;
        };
        if close < open {
            continue;
        }
        let name = unescape(chunk[open + 1..close].trim());
        if !id.is_empty() && !name.is_empty() {
            out.push((id.to_string(), name));
        }
    }
    out
}

impl DriverModule for Roku {
    fn discover(&self, _driver_id: &str, state: &Value, input: &Args) -> (SetupStep, Value) {
        self.flow(state, input)
    }

    fn setup(&self, _driver_id: &str, state: &Value, input: &Args) -> (SetupStep, Value) {
        self.flow(state, input)
    }

    fn on_command(
        &self,
        inst: &mut Instance,
        proxy: LocalId,
        cmd: &str,
        args: &Args,
    ) -> Vec<HostCall> {
        let Some(base) = Self::base(inst) else {
            return vec![HostCall::warn("roku: set the Address on this device first")];
        };

        // --- launcher and channels -------------------------------------------------------
        //
        // Core deliberately does not turn a launcher source into a d-pad Home press: not every
        // platform reaches its apps that way. Roku does, through ECP's documented Home key, so
        // that vendor-specific choice stays here.
        if cmd == "open_app_launcher" {
            return Self::keypress(inst, "Home").into_iter().collect();
        }

        if cmd == "launch_app" {
            let Some(want) = args.get("app").and_then(Value::as_str) else {
                return vec![HostCall::warn("roku: launch_app needs an app name")];
            };
            let Some(id) = Self::app_id(inst, want) else {
                // Saying which channels DO exist turns a dead end into a retry that works.
                let known: Vec<String> = inst
                    .scratch
                    .get("app_map")
                    .and_then(Value::as_object)
                    .map(|m| m.values().filter_map(|v| v.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                return vec![HostCall::warn(format!(
                    "roku: no channel matching `{want}`; installed: {}",
                    known.join(", ")
                ))];
            };

            let mut url = format!("{base}/launch/{id}");
            if let Some(content) = args.get("content_id").and_then(Value::as_str) {
                // ECP wants to be told what the id refers to, and gets it wrong when it is not.
                // This was hardcoded to `movie`, so every series deep link asked Roku to open a
                // film with a series id — which opens nothing and reports nothing. `season` and
                // `short` have no ECP spelling; `series` is the closest thing ECP has to either.
                let kind = match args.get("content_kind").and_then(Value::as_str) {
                    Some("series") | Some("season") => "series",
                    Some("episode") => "episode",
                    Some("live") => "live",
                    // Absent means the caller did not know, and `movie` is what Roku itself
                    // assumes for a bare content id.
                    _ => "movie",
                };
                url.push_str(&format!("?contentId={content}&mediaType={kind}"));
            }

            let name = inst
                .scratch
                .get("app_map")
                .and_then(|m| m.get(&id))
                .and_then(Value::as_str)
                .unwrap_or(want)
                .to_string();

            let mut a = Args::new();
            a.insert("app".into(), json!(name));
            let mut out = vec![
                HostCall::Http(HttpRequest::new("POST", url)),
                HostCall::notify(MEDIA, "app_changed", a),
            ];
            // The device would not say what it has installed, so this may land on nothing.
            // Better to warn than to report success for something that did not happen.
            if inst.scratch.get("apps_unconfirmed").and_then(Value::as_bool) == Some(true) {
                out.push(HostCall::Log {
                    level: "info".into(),
                    msg: format!(
                        "launched {name}; this Roku would not confirm its channel list, so \
                         nothing happens if it is not installed"
                    ),
                });
            }
            return out;
        }

        // --- everything else is a keypress ------------------------------------------------
        let key = match (proxy, cmd) {
            (_, "play") | (_, "pause") => "Play", // Roku has one toggle, not two keys
            (_, "stop") => "Home",
            (_, "skip_forward") => "Fwd",
            (_, "skip_back") => "Rev",
            (_, "scan_forward") => "Fwd",
            (_, "scan_reverse") => "Rev",
            (_, "search") => "Search",

            (_, "dpad") => {
                let Some(k) = args.get("key").and_then(Value::as_str) else {
                    return vec![HostCall::warn("roku: dpad needs a key")];
                };
                match k {
                    "up" => "Up",
                    "down" => "Down",
                    "left" => "Left",
                    "right" => "Right",
                    "select" => "Select",
                    "back" => "Back",
                    "home" | "menu" => "Home",
                    "info" => "Info",
                    other => return vec![HostCall::warn(format!("roku: no key `{other}`"))],
                }
            }

            (_, other) => return vec![HostCall::warn(format!("roku: unhandled `{other}`"))],
        };

        let mut out = Vec::new();
        out.extend(Self::keypress(inst, key));

        // Report what we know changed. Roku does not push state, so anything not stated here
        // waits for the next poll.
        match cmd {
            "play" => {
                let mut a = Args::new();
                a.insert("state".into(), json!("playing"));
                out.push(HostCall::notify(MEDIA, "transport_changed", a));
            }
            "pause" => {
                let mut a = Args::new();
                a.insert("state".into(), json!("paused"));
                out.push(HostCall::notify(MEDIA, "transport_changed", a));
            }
            "stop" => {
                let mut a = Args::new();
                a.insert("state".into(), json!("stopped"));
                out.push(HostCall::notify(MEDIA, "transport_changed", a));
            }
            _ => {}
        }
        out
    }

    fn on_event(
        &self,
        inst: &mut Instance,
        _control: LocalId,
        note: &str,
        args: &Args,
    ) -> Vec<HostCall> {
        if note != "http_response" {
            return Vec::new();
        }
        let Some(body) = args.get("body").and_then(Value::as_str) else {
            return Vec::new();
        };

        // A Roku set to "Limited" refuses to list or launch anything, and says so in plain
        // text rather than an error code. Without this the channel list is simply empty and
        // nothing explains why — which reads as a broken driver rather than a setting.
        if body.contains("not allowed in Limited mode") {
            // Launching still works in this mode, so offer the well-known channels rather
            // than nothing. They are unconfirmed: this device may not have them installed.
            let map: driver_sdk::serde_json::Map<String, Value> = WELL_KNOWN
                .iter()
                .map(|(id, name)| ((*id).to_string(), json!(name)))
                .collect();
            inst.scratch.insert("app_map".into(), Value::Object(map));
            inst.scratch.insert("apps_unconfirmed".into(), json!(true));

            let mut a = Args::new();
            a.insert(
                "apps".into(),
                json!(WELL_KNOWN.iter().map(|(_, n)| *n).collect::<Vec<_>>()),
            );
            return vec![
                HostCall::warn(LIMITED_MODE),
                HostCall::notify(MEDIA, "apps_changed", a),
            ];
        }

        // What this box is. `is-tv` is the whole question: a Roku TV grows a screen and a
        // Roku player does not, and nothing before this point can tell them apart — both
        // answer `roku:ecp` with the same SERVER banner.
        if body.contains("<device-info>") {
            if tag(body, "is-tv").as_deref() != Some("true") {
                return Vec::new();
            }
            let name = tag(body, "friendly-device-name")
                .or_else(|| tag(body, "user-device-name"))
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| "Screen".into());
            return vec![HostCall::Present {
                nodes: vec![Node {
                    node: SCREEN.into(),
                    // The set's own name for itself, which is what somebody called it when
                    // they set the television up.
                    name,
                    manufacturer: tag(body, "vendor-name").unwrap_or_else(|| "Roku".into()),
                    model: tag(body, "model-name").unwrap_or_default(),
                    kind: "tv".into(),
                    capabilities: [
                        ("has_discrete_power".to_string(), json!(true)),
                        ("has_discrete_input".to_string(), json!(true)),
                        ("has_volume".to_string(), json!(true)),
                        // Measured on the sets this has been run against: ECP is accepted
                        // while the panel is still coming up, and acted on when it is not.
                        ("warmup_ms".to_string(), json!(2000)),
                    ]
                    .into_iter()
                    .collect(),
                    ..Default::default()
                }],
            }];
        }

        // Order matters: an <active-app> document also contains an <app> element, so
        // checking for the list first would read "what is playing" as "everything installed"
        // and wipe the real list down to one entry.
        if body.contains("<active-app>") {
            if let Some((_, name)) = parse_apps(body).into_iter().next() {
                let mut a = Args::new();
                a.insert("app".into(), json!(name));
                return vec![HostCall::notify(MEDIA, "app_changed", a)];
            }
            return Vec::new();
        }

        // The channel list. This is what makes "watch Netflix" possible at all.
        if body.contains("<apps>") {
            let apps = parse_apps(body);
            if apps.is_empty() {
                return Vec::new();
            }
            let map: driver_sdk::serde_json::Map<String, Value> = apps
                .iter()
                .map(|(id, name)| (id.clone(), json!(name)))
                .collect();
            inst.scratch.insert("app_map".into(), Value::Object(map));

            // Every channel has artwork on the box itself: GET /query/icon/<appId> is the
            // tile the Roku draws on its own home screen. Report the URL alongside the name
            // and a control surface can show the logo people actually look for. The HDMI
            // inputs are launchable "channels" with no artwork; they get an empty string.
            let base = Self::base(inst).unwrap_or_default();
            let icons: Vec<String> = apps
                .iter()
                .map(|(id, _)| {
                    if base.is_empty() || id.starts_with("tvinput.") {
                        String::new()
                    } else {
                        format!("{base}/query/icon/{id}")
                    }
                })
                .collect();

            // A Roku TV lists its physical inputs in the same breath as its channels, so the
            // set says how many HDMI ports it has and the manifest's guess of two can go.
            let connections: Vec<ConnectionDecl> = apps
                .iter()
                .filter_map(|(id, name)| {
                    Some(ConnectionDecl {
                        id: connection_id(id)?,
                        proxy: Some(NODE),
                        dir: Direction::Consumer,
                        class: signal_class(id).into(),
                        name: name.clone(),
                    })
                })
                .collect();

            let names: Vec<String> = apps.into_iter().map(|(_, n)| n).collect();
            let mut a = Args::new();
            a.insert("apps".into(), json!(names));
            a.insert("app_icons".into(), json!(icons));
            let mut out = vec![HostCall::notify(MEDIA, "apps_changed", a)];
            // Aimed at the screen, whose inputs they are. A Roku player has no inputs and
            // lists none, and an empty list would claim it has none rather than that it never
            // said — so only a set that reported some sends this at all.
            if !connections.is_empty() {
                out.push(HostCall::ForNode {
                    node: SCREEN.into(),
                    calls: vec![HostCall::Connections { connections }],
                });
            }
            return out;
        }

        Vec::new()
    }

    /// The screen's own commands: power, volume, and which input it is showing.
    ///
    /// `inst` is the Roku's, not the screen's — which is what makes this work at all, since
    /// the address lives on the parent and the screen is the same box at the same address.
    fn on_node_command(
        &self,
        inst: &mut Instance,
        node: &str,
        _kind: &str,
        cmd: &str,
        args: &Args,
    ) -> Vec<HostCall> {
        if node != SCREEN {
            return vec![HostCall::warn(format!("roku: `{node}` is not a node this driver made"))];
        }
        let Some(base) = Self::base(inst) else {
            return vec![HostCall::warn("roku: set the Address on this device first")];
        };

        if cmd == "set_input" {
            // Roku TV models inputs as launchable "channels" too. The connection id is not
            // the HDMI number: 1001 is HDMI 1, and interpolating it straight into the channel
            // id asked for `tvinput.hdmi1001`, which no set has — so switching inputs never
            // worked and said nothing about it.
            let Some(connection) = args.get("connection").and_then(Value::as_u64) else {
                return vec![HostCall::warn("roku: set_input needs a connection")];
            };
            let Some(input) = tvinput_for(connection) else {
                return vec![HostCall::warn(format!("roku: no such connection {connection}"))];
            };
            let mut a = Args::new();
            a.insert("connection".into(), json!(connection));
            return vec![
                HostCall::Http(HttpRequest::new("POST", format!("{base}/launch/{input}"))),
                for_screen("input_changed", a),
            ];
        }

        let key = match cmd {
            "on" => "PowerOn",
            "off" => "PowerOff",
            "power_toggle" => "Power",
            "volume_up" => "VolumeUp",
            "volume_down" => "VolumeDown",
            "mute_toggle" | "set_mute" => "VolumeMute",
            other => return vec![HostCall::warn(format!("roku: unhandled `{other}`"))],
        };

        let mut out: Vec<HostCall> = Self::keypress(inst, key).into_iter().collect();
        // Roku does not push state, so anything not stated here waits for the next poll.
        if let "on" | "off" = cmd {
            let mut a = Args::new();
            a.insert("on".into(), json!(cmd == "on"));
            out.push(for_screen("power_changed", a));
        }
        out
    }

    fn on_bind(&self, inst: &mut Instance) -> Vec<HostCall> {
        let mut out = Vec::new();
        let mut a = Args::new();
        a.insert("online".into(), json!(true));
        out.push(HostCall::notify(MEDIA, "online_changed", a));
        // Whether this box has a screen. Asked on every bind rather than remembered from
        // setup, because a driver that only learned it once would never grow a screen on a
        // controller whose project was imported, restored, or written by hand.
        out.extend(Self::get(inst, "/query/device-info"));
        // Read the channel list before anyone asks for one.
        out.extend(Self::get(inst, "/query/apps"));
        out.extend(Self::get(inst, "/query/active-app"));
        out
    }
}


// ---------------------------------------------------------------------------------------
// Setup flow
// ---------------------------------------------------------------------------------------

/// Pull one tag out of an ECP XML document. The documents are flat, single-purpose, and from
/// a device on the LAN — an XML parser would be more dependency than this deserves.
pub fn tag(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim().to_string())
}

impl Roku {
    /// Offer whatever answered the SSDP search, and let an address be typed anyway.
    ///
    /// Roku does not advertise over mDNS — it answers `M-SEARCH` for `roku:ecp`, which is
    /// what the manifest declares and what core searched for before calling us.
    /// Ask each responder what it is called, one at a time.
    ///
    /// SSDP only reports an address and a firmware string, so an unenriched list is two rows
    /// reading "Roku/15.2.4" and nothing to tell them apart. `/query/device-info` carries the
    /// name someone actually gave the television, which is the only useful label.
    fn enrich(state: &Value) -> Option<(SetupStep, Value)> {
        let candidates = state.get("ssdp_candidates")?.as_array()?;
        let at = state.get("enrich_at").and_then(Value::as_u64).unwrap_or(0) as usize;
        if at >= candidates.len() {
            return None;
        }
        let address = candidates.get(at)?.get("address")?.as_str()?.to_string();
        Some((
            SetupStep::Fetch {
                request: HttpRequest::new(
                    "GET",
                    format!("http://{address}:8060/query/device-info"),
                ),
                note: format!("asking {address} what it is called"),
            },
            {
                let mut next = state.clone();
                next["enrich_at"] = json!(at + 1);
                next["phase"] = json!("naming");
                next
            },
        ))
    }

    fn ask_for_address(state: &Value) -> (SetupStep, Value) {
        let typed = Field {
            name: "address".into(),
            label: "Address".into(),
            kind: "string".into(),
            help: "for example 192.168.1.60".into(),
            default: None,
            options: Vec::new(),
            required: true,
        };

        let found: Vec<&Value> = state
            .get("ssdp_candidates")
            .and_then(Value::as_array)
            .map(|v| v.iter().collect())
            .unwrap_or_default();

        if found.is_empty() {
            return (
                SetupStep::Form {
                    title: "Find your Roku".into(),
                    body: "Nothing answered on the network, so enter its address — on the \
                           device, Settings → Network → About. Nothing needs enabling first; \
                           a Roku answers by default unless someone has turned off external \
                           control."
                        .into(),
                    fields: vec![typed],
                },
                json!({ "phase": "probe" }),
            );
        }

        let rows: Vec<PickRow> = found
            .iter()
            .filter_map(|f| {
                let address = f.get("address")?.as_str()?.to_string();
                // SERVER reads "Roku/12.5.5 UPnP/1.0 Roku/12.5.5"; the first token is enough.
                // The name someone gave it, falling back to the firmware string only when
                // the device would not say.
                let label = f
                    .get("friendly")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        f.get("server")
                            .and_then(Value::as_str)
                            .unwrap_or("Roku")
                            .split_whitespace()
                            .next()
                            .unwrap_or("Roku")
                            .to_string()
                    });
                let kind = f
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        if f.get("is_tv").and_then(Value::as_bool) == Some(true) {
                            "Roku TV".into()
                        } else {
                            "Roku".into()
                        }
                    });
                // The USN carries a stable id, which is how two identical televisions are
                // told apart before either has been named.
                let id = f
                    .get("usn")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .rsplit(':')
                    .next()
                    .unwrap_or("")
                    .to_string();
                Some(PickRow {
                    value: address.clone(),
                    cells: vec![label, kind, address],
                    note: if id.is_empty() { String::new() } else { format!("serial {id}") },
                })
            })
            .collect();

        (
            SetupStep::Pick {
                title: format!(
                    "Found {} Roku device{}",
                    rows.len(),
                    if rows.len() == 1 { "" } else { "s" }
                ),
                body: "Pick one. Its model decides whether it offers a screen as well as \
                       apps — a TV does, a stick does not."
                    .into(),
                columns: vec!["Name".into(), "Model".into(), "Address".into()],
                rows,
                field: "address".into(),
                manual: Some(typed),
            },
            json!({ "phase": "probe" }),
        )
    }

    fn flow(&self, state: &Value, input: &Args) -> (SetupStep, Value) {
        let phase = state.get("phase").and_then(Value::as_str).unwrap_or("start");
        let address = state
            .get("address")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| input.get("address").and_then(Value::as_str).map(str::to_string));

        match phase {
            "start" => {
                // Put a name to each responder before showing the list.
                match Self::enrich(state) {
                    Some(next) => next,
                    None => Self::ask_for_address(state),
                }
            }

            // One device-info reply. Record the name and move to the next, or show the list.
            "naming" => {
                let at = state.get("enrich_at").and_then(Value::as_u64).unwrap_or(0) as usize;
                let mut next = state.clone();
                if at > 0
                    && let Some(xml) = input.get("response").and_then(Value::as_str)
                {
                    let name = tag(xml, "user-device-name")
                        .filter(|s| !s.is_empty())
                        .or_else(|| tag(xml, "friendly-device-name"))
                        .or_else(|| tag(xml, "model-name"));
                    let is_tv = tag(xml, "is-tv").as_deref() == Some("true");
                    if let Some(list) = next
                        .get_mut("ssdp_candidates")
                        .and_then(Value::as_array_mut)
                        .and_then(|a| a.get_mut(at - 1))
                    {
                        if let Some(name) = name {
                            list["friendly"] = json!(name);
                        }
                        list["is_tv"] = json!(is_tv);
                        // `model-name` on a Roku TV is the panel's part number — "100012587"
                        // tells nobody anything. `friendly-model-name` is what the maker
                        // calls it: "onn•Roku TV".
                        list["model"] = json!(
                            tag(xml, "friendly-model-name")
                                .filter(|s| !s.is_empty())
                                .or_else(|| tag(xml, "model-name"))
                                .unwrap_or_default()
                        );
                    }
                }
                match Self::enrich(&next) {
                    Some(step) => step,
                    None => Self::ask_for_address(&next),
                }
            }

            "probe" => {
                let Some(address) = address else {
                    return Self::ask_for_address(state);
                };
                let address = address.trim().trim_end_matches(":8060").to_string();
                (
                    SetupStep::Fetch {
                        request: HttpRequest::new(
                            "GET",
                            format!("http://{address}:8060/query/device-info"),
                        ),
                        note: "asking the Roku what it is".into(),
                    },
                    // Every arm rebuilds state from scratch, so anything that has to outlive
                    // one transition has to be re-stated here. `nagged` is the only such flag:
                    // dropping it sends the Limited-mode notice round forever.
                    json!({
                        "phase": "probed", "address": address,
                        "nagged": state.get("nagged").and_then(Value::as_bool).unwrap_or(false),
                    }),
                )
            }

            "probed" => {
                let address = address.unwrap_or_default();
                let xml = state
                    .get("info")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| input.get("response").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_default();
                if !xml.contains("<device-info>") {
                    return (
                        SetupStep::Failed {
                            reason: format!(
                                "{address} did not answer as a Roku. Check the address under \
                                 Settings → Network → About."
                            ),
                        },
                        Value::Null,
                    );
                }

                let model = tag(&xml, "model-name").unwrap_or_else(|| "Roku".into());
                let friendly = tag(&xml, "user-device-name")
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| model.clone());
                // A Roku TV has a screen to control; a stick or box does not, and offering
                // volume and inputs it cannot do would be a lie.
                let is_tv = tag(&xml, "is-tv").as_deref() == Some("true")
                    || tag(&xml, "device-type").as_deref() == Some("tv");
                let power = tag(&xml, "power-mode").unwrap_or_else(|| "unknown".into());

                // Reachability is not enough: a Roku can answer device-info and still refuse
                // every useful command. Ask for the channel list now, while there is somewhere
                // sensible to say so.
                if state.get("checked_control").and_then(Value::as_bool) != Some(true) {
                    return (
                        SetupStep::Fetch {
                            request: HttpRequest::new(
                                "GET",
                                format!("http://{address}:8060/query/apps"),
                            ),
                            note: "checking external control".into(),
                        },
                        json!({
                            "phase": "probed", "address": address, "checked_control": true,
                            "info": xml,
                            "nagged": state.get("nagged").and_then(Value::as_bool)
                                           .unwrap_or(false),
                        }),
                    );
                }

                let limited = input
                    .get("response")
                    .and_then(Value::as_str)
                    .is_some_and(|r| r.contains("not allowed in Limited mode"));

                // Say something once and offer to look again. Not twice: Limited control
                // blocks *listing*, not launching, so a device stuck this way is still worth
                // adding — refusing to add it would be a worse outcome than a channel list
                // that is a good guess.
                if limited && state.get("nagged").and_then(Value::as_bool) != Some(true) {
                    return (
                        SetupStep::Instruct {
                            title: format!("{friendly} is set to Limited control"),
                            body: format!(
                                "{LIMITED_MODE}\n\nYou can add it either way — it will still \
                                 launch channels and take volume. Only the list of what is \
                                 installed is refused."
                            ),
                            continue_label: "Check again".into(),
                        },
                        json!({ "phase": "probe", "address": address, "nagged": true }),
                    );
                }

                (
                    SetupStep::Choose {
                        title: format!("Found {friendly}"),
                        body: if is_tv {
                            "This is a Roku TV. Its screen arrives beside the streamer once it \
                             is added — with volume, power, and the inputs the set actually \
                             reports having."
                                .into()
                        } else {
                            "This is a Roku player, so it offers the streamer only. It has no \
                             volume or inputs of its own."
                                .to_string()
                        },
                        options: vec![Candidate {
                            label: friendly,
                            kind: model,
                            // One driver either way. Whether there is a screen is settled
                            // after adoption, by the box itself — see `SCREEN`.
                            driver_id: "roku.player".into(),
                            properties: [
                                ("Address".to_string(), json!(address)),
                                ("Port".to_string(), json!(8060)),
                            ]
                            .into_iter()
                            .collect(),
                            verified: if limited {
                                format!(
                                    "answered ECP, power {power} — set to Limited control, so \
                                     its channel list is a guess"
                                )
                            } else {
                                format!("answered ECP, power {power}")
                            },
                                                    ..Default::default()
                        }],
                        multiple: false,
                    },
                    json!({ "phase": "chosen", "address": address }),
                )
            }

            "chosen" => {
                let devices: Vec<Candidate> = input
                    .get("chosen")
                    .and_then(|c| driver_sdk::serde_json::from_value(c.clone()).ok())
                    .unwrap_or_default();
                (SetupStep::done(devices), Value::Null)
            }

            other => (
                SetupStep::Failed {
                    reason: format!("unknown setup phase `{other}`"),
                },
                Value::Null,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tv() -> Instance {
        let mut inst = Instance::default();
        inst.properties.insert("Address".into(), json!("10.0.0.5"));
        inst
    }

    /// A Roku TV grows a screen; a Roku player does not. Nothing before this can tell them
    /// apart — both answer `roku:ecp` with the same SERVER banner, and both are this driver.
    #[test]
    fn a_television_presents_a_screen_and_a_player_presents_nothing() {
        let driver = Roku;
        let reply = |xml: &str| {
            let mut a = Args::new();
            a.insert("body".into(), json!(xml));
            driver.on_event(&mut tv(), 0, "http_response", &a)
        };

        // Recorded from a real set at 192.168.1.157.
        let calls = reply(
            r#"<device-info><vendor-name>Hisense</vendor-name><model-name>40H4030</model-name>
               <is-tv>true</is-tv><is-stick>false</is-stick>
               <friendly-device-name>40" Hisense Roku TV</friendly-device-name></device-info>"#,
        );
        let [HostCall::Present { nodes }] = calls.as_slice() else {
            panic!("expected a screen, got {calls:?}");
        };
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].node, SCREEN);
        assert_eq!(nodes[0].kind, "tv");
        assert_eq!(nodes[0].name, "40\" Hisense Roku TV", "what its owner called it");
        assert_eq!(nodes[0].manufacturer, "Hisense");
        assert_eq!(nodes[0].capabilities["has_volume"], json!(true));

        // A stick. Same driver, same protocol, no panel attached to it.
        let calls = reply(
            r#"<device-info><vendor-name>Roku</vendor-name><model-name>Streaming Stick 4K</model-name>
               <is-tv>false</is-tv><is-stick>true</is-stick></device-info>"#,
        );
        assert!(
            calls.is_empty(),
            "a player has no screen, and presenting an empty one would offer a television \
             nobody owns — got {calls:?}",
        );
    }

    /// The connection id is not the HDMI number. Interpolating it straight into the channel id
    /// asked a real set for `tvinput.hdmi1001`, which it does not have — so switching inputs
    /// did nothing, and said nothing, for as long as this driver has shipped.
    #[test]
    fn set_input_maps_the_connection_id_to_the_channel_id() {
        let driver = Roku;
        let mut inst = tv();
        let mut a = Args::new();
        a.insert("connection".into(), json!(1002u64));
        let calls = driver.on_node_command(&mut inst, SCREEN, "tv", "set_input", &a);
        let [HostCall::Http(req), HostCall::ForNode { node, .. }] = calls.as_slice() else {
            panic!("expected a launch and a notify aimed at the screen, got {calls:?}");
        };
        assert_eq!(node, SCREEN);
        assert!(req.url.ends_with("/launch/tvinput.hdmi2"), "{}", req.url);
    }

    #[test]
    fn the_app_launcher_uses_rokus_home_key() {
        let calls = Roku.on_command(&mut tv(), MEDIA, "open_app_launcher", &Args::new());
        let [HostCall::Http(req)] = calls.as_slice() else {
            panic!("expected one keypress, got {calls:?}");
        };
        assert_eq!(req.method, "POST");
        assert!(req.url.ends_with("/keypress/Home"), "{}", req.url);
    }

    #[test]
    fn set_input_refuses_a_connection_no_roku_has() {
        let driver = Roku;
        let mut inst = tv();
        let mut a = Args::new();
        a.insert("connection".into(), json!(7u64));
        let calls = driver.on_node_command(&mut inst, SCREEN, "tv", "set_input", &a);
        assert!(matches!(calls.as_slice(), [HostCall::Log { level, .. }] if level == "warn"));
    }

    /// A Roku TV lists its inputs among its channels, so the set says how many HDMI ports it
    /// has and the manifest's guess of two stops being the answer.
    #[test]
    fn the_channel_list_reports_the_sets_real_inputs() {
        let driver = Roku;
        let mut inst = tv();
        let xml = r#"<apps>
            <app id="12">Netflix</app>
            <app id="tvinput.hdmi1">HDMI 1</app>
            <app id="tvinput.hdmi2">HDMI 2</app>
            <app id="tvinput.hdmi3">HDMI 3</app>
            <app id="tvinput.tuner">Antenna TV</app>
            <app id="837">YouTube</app>
        </apps>"#;
        let mut a = Args::new();
        a.insert("body".into(), json!(xml));
        let calls = driver.on_event(&mut inst, 0, "http_response", &a);
        // Aimed at the screen: they are the screen's jacks, not the streamer's.
        let Some(connections) = calls.iter().find_map(|c| match c {
            HostCall::ForNode { node, calls } if node == SCREEN => {
                calls.iter().find_map(|c| match c {
                    HostCall::Connections { connections } => Some(connections.clone()),
                    _ => None,
                })
            }
            _ => None,
        }) else {
            panic!("expected connections aimed at the screen, got {calls:?}");
        };

        let ids: Vec<LocalId> = connections.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![1001, 1002, 1003, 1201]);
        assert!(!ids.contains(&1004), "this set has three HDMI ports");
        assert!(
            connections
                .iter()
                .all(|c| c.dir == Direction::Consumer && c.proxy == Some(NODE))
        );
        assert_eq!(connections.iter().find(|c| c.id == 1201).unwrap().class, "RF_UHF_VHF");
        // Channels are not jacks.
        assert_eq!(connections.len(), 4);
    }

    /// A player has no inputs and lists none. An empty list would claim it has none rather
    /// than that it never said, and that is a different statement — see `HostCall::Connections`.
    #[test]
    fn a_player_reports_no_connections_at_all() {
        let driver = Roku;
        let mut inst = tv();
        let mut a = Args::new();
        a.insert("body".into(), json!(r#"<apps><app id="12">Netflix</app></apps>"#));
        let calls = driver.on_event(&mut inst, 0, "http_response", &a);
        assert!(
            !calls.iter().any(|c| matches!(c, HostCall::Connections { .. })),
            "got {calls:?}"
        );
    }

    #[test]
    fn ids_round_trip_between_the_channel_id_and_the_connection() {
        assert_eq!(connection_id("tvinput.hdmi3"), Some(1003));
        assert_eq!(tvinput_for(1003).as_deref(), Some("tvinput.hdmi3"));
        assert_eq!(connection_id("12"), None, "Netflix is not a jack");
    }
}

export_driver!(Roku);
