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
//! # Two manifests, one package
//!
//! `roku.player` is a streamer. `roku.tv` is a television with that same streamer inside it, and
//! it leads with the `tv` proxy for that reason: somebody bought a television, and filing the
//! screen underneath its own launcher reads as an accessory of the app menu. Nothing on the wire
//! tells the two apart — both answer
//! `roku:ecp` on 8060 with the same SERVER banner — so only `roku.player` declares discovery,
//! and setup reads `is-tv` from `/query/device-info` before offering anything and hands back
//! whichever id the box says it is. Same shape `apple-tv` uses for its IR sibling.
//!
//! The screen used to be a presented child *node* instead, on the argument that having a panel
//! is a fact about the unit rather than the product. It is a fact about the model, setup already
//! knows it, and as a node it became a second device beside the streamer for somebody to notice
//! and adopt — where every other television in Juno, `vizio.tv` included, is one device with a
//! `media_player` and a `tv` on it.
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

/// The screen, on the manifest that has one — and the proxy that manifest leads with.
///
/// A second proxy rather than a presented child node, which is what this used to be. Whether a
/// Roku has a panel is a fact about the *model*, and setup already asks the box — `is-tv` in
/// `/query/device-info`, before anything is adopted — so it can hand back `roku.tv` or
/// `roku.player` and the answer is settled in the manifest where a proxy belongs. As a node it
/// was a second device the installer had to notice and adopt separately, sitting beside the
/// streamer in the tree rather than inside it, and nothing else in Juno models a television
/// that way; `vizio.tv` is one device with two proxies and this is now the same shape.
///
/// A `roku.player` has no proxy 2 at all, so a command aimed at one is refused below rather
/// than sent to a stick that would ignore it.
const TV: LocalId = 2;

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
    /// An absolute base, for the one thing that still needs one: the artwork URLs handed to a
    /// screen, which a browser dereferences rather than core. Requests go out as bare paths and
    /// are resolved against this device by core — see `HostCall::Http`.
    fn base(inst: &Instance) -> Option<String> {
        let addr = inst.property("Address").as_str()?.trim().to_string();
        if addr.is_empty() {
            return None;
        }
        let port = inst.property("Port").as_u64().unwrap_or(8060);
        Some(format!("http://{addr}:{port}"))
    }

    fn keypress(key: &str) -> HostCall {
        HostCall::Http(HttpRequest::new("POST", format!("/keypress/{key}")))
    }

    fn get(path: &str) -> HostCall {
        HostCall::Http(HttpRequest::new("GET", path))
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
        // --- launcher and channels -------------------------------------------------------
        //
        // Core deliberately does not turn a launcher source into a d-pad Home press: not every
        // platform reaches its apps that way. Roku does, through ECP's documented Home key, so
        // that vendor-specific choice stays here.
        if cmd == "open_app_launcher" {
            return vec![Self::keypress("Home")];
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

            let mut url = format!("/launch/{id}");
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

        // --- the screen -------------------------------------------------------------------
        //
        // Same instance, same address: a Roku TV is one box answering one ECP port, and the
        // panel is reached with the same keys as everything else. The proxy is what separates
        // them, so `volume_up` on the streamer is refused rather than quietly turning the
        // television up.
        if proxy == TV && cmd == "set_input" {
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
                HostCall::Http(HttpRequest::new("POST", format!("/launch/{input}"))),
                HostCall::notify(TV, "input_changed", a),
            ];
        }

        // --- everything else is a keypress ------------------------------------------------
        let key = match (proxy, cmd) {
            (TV, "on") => "PowerOn",
            (TV, "off") => "PowerOff",
            (TV, "power_toggle") => "Power",
            (TV, "volume_up") => "VolumeUp",
            (TV, "volume_down") => "VolumeDown",
            (TV, "mute_toggle") => "VolumeMute",

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
        out.push(Self::keypress(key));
        // Having pressed it, ask what happened. A power key with nothing behind it leaves the
        // house believing whatever it believed before — and `Runtime::showing_on_sink` refuses
        // to route a d-pad press at a screen it thinks is off, so a television just turned on
        // would stop taking arrows until the next poll came round.
        //
        // It may still read the old value: a set takes a moment to wake, which is the same fact
        // `warmup_ms` states from the pathfinder's side. A stale reading the next poll corrects
        // is the honest failure; a claim that stays wrong while the set sits dark is not.
        if matches!(cmd, "on" | "off" | "power_toggle") {
            out.push(Self::get("/query/device-info"));
        }

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
            // Nothing optimistic, power included. Sending `PowerOn` is not evidence a
            // television came on: the set may be unplugged, on a different input, or refusing
            // ECP while it wakes — and a house that believed the command was the outcome would
            // then route a room through a screen that is dark. `power-mode` in
            // `/query/device-info` is the set's own answer, read on every bind and every poll,
            // and it is the only thing here that writes `on`.
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

        // Where the set actually stands, which is the only honest source for it.
        //
        // `is-tv` used to be the whole reason this was read — it decided whether to present a
        // screen — and that moved to setup. What is left is better: `power-mode` is the set
        // reporting its own power, so the house can stop guessing. Measured on a Hisense Roku
        // TV: `PowerOn` while it is on, `Ready` in standby.
        //
        // `DisplayOff` is a Roku player with its video output asleep and `Headless` is one with
        // no display attached at all; neither is a television that is on, and neither is one
        // that was turned off either — but for a `tv` binding there are only two answers and
        // "not showing a picture" is the truer of them.
        if body.contains("<device-info>") {
            let Some(mode) = tag(body, "power-mode") else {
                return Vec::new();
            };
            let mut a = Args::new();
            a.insert("on".into(), json!(mode.eq_ignore_ascii_case("PowerOn")));
            return vec![HostCall::notify(TV, "power_changed", a)];
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
                        proxy: Some(TV),
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
            // The screen's jacks, not the streamer's — hence `proxy: Some(TV)` above. A Roku
            // player has no inputs and lists none, and an empty list would pin "this device has
            // no connections" on the project rather than leaving it never-said. So only a set
            // that reported some sends this at all.
            if !connections.is_empty() {
                out.push(HostCall::Connections { connections });
            }
            return out;
        }

        Vec::new()
    }

    fn on_bind(&self, _inst: &mut Instance) -> Vec<HostCall> {
        let mut out = Vec::new();
        let mut a = Args::new();
        a.insert("online".into(), json!(true));
        out.push(HostCall::notify(MEDIA, "online_changed", a));
        // Where the set stands. Asked on every bind — and a bind is also every poll, see the
        // `Poll interval` property — because nothing else will say: ECP pushes nothing, and a
        // television turned on by its own remote is a television this house believes is off
        // until it asks.
        out.push(Self::get("/query/device-info"));
        // Read the channel list before anyone asks for one. It carries this set's real
        // inputs as well as its channels, which is what replaces the manifest's guess.
        out.push(Self::get("/query/apps"));
        out.push(Self::get("/query/active-app"));
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

    /// The one responder in the seed, when there is exactly one.
    ///
    /// Core seeds a flow started from a Discovery row with the candidates for that one address,
    /// and a flow started cold with everything a search turned up — so one is the row somebody
    /// pressed and several is a real question. `None` for either of the other two cases.
    fn the_only_candidate(state: &Value) -> Option<String> {
        let found = state.get("ssdp_candidates")?.as_array()?;
        let [only] = found.as_slice() else { return None };
        let address = only.get("address")?.as_str()?.trim();
        (!address.is_empty()).then(|| address.trim_end_matches(":8060").to_string())
    }

    /// Ask a box what it is. The one request every way into this flow ends up making, because
    /// `is-tv` is what decides which of the two manifests it is offered as.
    fn probe_for(address: &str) -> SetupStep {
        SetupStep::Fetch {
            request: HttpRequest::new("GET", format!("http://{address}:8060/query/device-info")),
            note: "asking the Roku what it is".into(),
        }
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
                // One responder is the one that was pressed. Reaching this flow from a row in
                // Discovery means the search has already happened and its answer is the thing on
                // screen — so asking somebody to find it in a list of one, and then to confirm
                // it on a second screen, is asking the same question three times. Straight to
                // the probe, which is the only part that has anything left to learn.
                //
                // Told from a general search by there being exactly one: core seeds a row with
                // the candidates for that one address, and a search with everything it found.
                // Several is a genuine question and still gets the list.
                if let Some(only) = Self::the_only_candidate(state) {
                    return (
                        Self::probe_for(&only),
                        json!({ "phase": "probed", "address": only, "chosen": true }),
                    );
                }
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
                    Self::probe_for(&address),
                    // Every arm rebuilds state from scratch, so anything that has to outlive one
                    // transition has to be re-stated here. `nagged` stops the Limited-mode notice
                    // going round forever; `chosen` remembers that somebody already pointed at
                    // this box, which a retry after that notice would otherwise forget — and the
                    // confirmation screen would come back for a device already chosen twice.
                    json!({
                        "phase": "probed", "address": address,
                        "nagged": state.get("nagged").and_then(Value::as_bool).unwrap_or(false),
                        "chosen": state.get("chosen").and_then(Value::as_bool).unwrap_or(false),
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
                            // Carried, like everything else that has to outlive a transition:
                            // dropping it here put the confirmation screen back in front of a
                            // box somebody had already pressed, which is the whole thing this
                            // flag exists to prevent.
                            "chosen": state.get("chosen").and_then(Value::as_bool)
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
                        json!({
                            "phase": "probe", "address": address, "nagged": true,
                            "chosen": state.get("chosen").and_then(Value::as_bool)
                                           .unwrap_or(false),
                        }),
                    );
                }

                let offer = Candidate {
                    label: friendly.clone(),
                    kind: model.clone(),
                    // The one place the two manifests are told apart, and the reason only
                    // `roku.player` declares discovery: both are `roku:ecp` on 8060 and the wire
                    // cannot tell them apart, but by here the box has been asked. See `TV`.
                    driver_id: if is_tv { "roku.tv" } else { "roku.player" }.into(),
                    properties: [
                        ("Address".to_string(), json!(address)),
                        ("Port".to_string(), json!(8060)),
                    ]
                    .into_iter()
                    .collect(),
                    verified: if limited {
                        format!(
                            "answered ECP, power {power} — set to Limited control, so its \
                             channel list is a guess"
                        )
                    } else {
                        format!("answered ECP, power {power}")
                    },
                    ..Default::default()
                };

                // Somebody pressed this box in Discovery. Offering it back as the only row of a
                // list to choose from is asking the same question a second time — the search
                // already happened, and its answer is what started this. Straight to adding it,
                // the way `vizio.tv` does after a pairing names one television three times.
                //
                // Only where the box was pointed at. A flow that started cold, or one where the
                // list had several rows, still confirms: there the question is real.
                if state.get("chosen").and_then(Value::as_bool) == Some(true) {
                    return (SetupStep::done(vec![offer]), Value::Null);
                }

                (
                    SetupStep::Choose {
                        title: format!("Found {friendly}"),
                        body: if is_tv {
                            "This is a Roku TV, so it is added as a streamer and a screen \
                             together — volume, power, and the inputs the set actually reports \
                             having, on one device."
                                .into()
                        } else {
                            "This is a Roku player, so it offers the streamer only. It has no \
                             volume or inputs of its own."
                                .to_string()
                        },
                        options: vec![offer],
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

    /// A Roku TV is added as `roku.tv`, a stick as `roku.player`, and the box is what decides.
    ///
    /// The whole reason the two manifests can exist without asking whoever is adding one: both
    /// answer `roku:ecp` with the same SERVER banner, so discovery cannot tell them apart — but
    /// setup has already read `/query/device-info` by the time it offers anything.
    #[test]
    fn setup_offers_the_manifest_the_box_says_it_is() {
        let offered = |xml: &str| {
            let mut input = Args::new();
            input.insert("response".into(), json!(xml));
            let state = json!({
                "phase": "probed", "address": "10.0.0.5", "checked_control": true, "info": xml,
            });
            match Roku.setup("roku.player", &state, &input).0 {
                SetupStep::Choose { options, .. } => options[0].driver_id.clone(),
                other => panic!("expected a choice, got {other:?}"),
            }
        };

        // Recorded from a real set at 192.168.1.157.
        assert_eq!(
            offered(
                r#"<device-info><vendor-name>Hisense</vendor-name><model-name>40H4030</model-name>
                   <is-tv>true</is-tv><is-stick>false</is-stick>
                   <friendly-device-name>40" Hisense Roku TV</friendly-device-name></device-info>"#,
            ),
            "roku.tv",
        );

        // A stick. Same driver package, same protocol, no panel attached to it — and offering
        // the `tv` proxy anyway would put volume and inputs on a television nobody owns.
        assert_eq!(
            offered(
                r#"<device-info><vendor-name>Roku</vendor-name><model-name>Streaming Stick 4K</model-name>
                   <is-tv>false</is-tv><is-stick>true</is-stick></device-info>"#,
            ),
            "roku.player",
        );
    }

    /// Power is what the set says it is, never what it was told to be.
    ///
    /// Sending `PowerOn` is not evidence a television came on — it may be unplugged, or
    /// refusing ECP while it wakes — and a house that took the command for the outcome would
    /// route a room through a dark screen and report it as working. ECP pushes nothing, so the
    /// only honest source is `power-mode` in `/query/device-info`, read on every bind and, via
    /// the `Poll interval` property, on every poll.
    #[test]
    fn power_comes_from_the_set_rather_than_from_having_asked() {
        let driver = Roku;

        // Turning it on says nothing about power. Measured on a real set: ECP accepts the key
        // while the panel is still coming up, and acts on it when it is not.
        let calls = driver.on_command(&mut tv(), TV, "on", &Args::new());
        assert!(
            !calls.iter().any(|c| matches!(c, HostCall::Notify { name, .. } if name == "power_changed")),
            "the command is not the answer: {calls:?}",
        );

        // The set saying where it stands is. `PowerOn` while on, `Ready` in standby — both read
        // off a Hisense Roku TV.
        let reading = |xml: &str| {
            let mut a = Args::new();
            a.insert("body".into(), json!(xml));
            match driver.on_event(&mut tv(), 0, "http_response", &a).as_slice() {
                [HostCall::Notify { proxy, name, args }] if name == "power_changed" => {
                    assert_eq!(*proxy, TV, "the screen's power, not the streamer's");
                    args.get("on").and_then(Value::as_bool)
                }
                other => panic!("expected one power_changed, got {other:?}"),
            }
        };
        assert_eq!(reading("<device-info><power-mode>PowerOn</power-mode></device-info>"), Some(true));
        assert_eq!(reading("<device-info><power-mode>Ready</power-mode></device-info>"), Some(false));
        // A player with its output asleep is not a television that is on.
        assert_eq!(
            reading("<device-info><power-mode>DisplayOff</power-mode></device-info>"),
            Some(false),
        );

        // And pressing power asks what it did, rather than leaving the house on its last
        // belief until the poll comes round — `Runtime::showing_on_sink` will not route a
        // d-pad press at a screen it thinks is off.
        let calls = driver.on_command(&mut tv(), TV, "on", &Args::new());
        assert!(
            calls.iter().any(|c| matches!(c, HostCall::Http(r) if r.url.contains("device-info"))),
            "asked, then asked what happened: {calls:?}",
        );

        // And every bind asks, because a poll is a bind.
        let calls = driver.on_bind(&mut tv());
        assert!(
            calls.iter().any(|c| matches!(c, HostCall::Http(r) if r.url.contains("device-info"))),
            "nothing else would ever ask: {calls:?}",
        );
    }

    /// Pressing a Roku in Discovery adds it, without asking twice more.
    ///
    /// The search has already happened and its answer is the row that was pressed — so a list of
    /// one to pick from, and then a screen to confirm the pick, are the same question asked
    /// three times. Only the probe is left, because `is-tv` decides which of the two manifests
    /// it is offered as and nothing before that point knows.
    #[test]
    fn a_roku_pressed_in_discovery_is_not_offered_back_as_a_list_of_one() {
        let seed = json!({
            "ssdp_candidates": [{ "address": "192.168.1.157", "usn": "uuid:roku:ecp:X" }],
        });
        let (step, state) = Roku.discover("roku.player", &seed, &Args::new());
        let SetupStep::Fetch { request, .. } = step else {
            panic!("straight to the probe, not a list: {step:?}");
        };
        assert!(request.url.ends_with("/query/device-info"), "{}", request.url);

        // The set answers, and it is added — no `Choose` in between.
        let mut probed = Args::new();
        probed.insert(
            "response".into(),
            json!(r#"<device-info><is-tv>true</is-tv><model-name>40H4030</model-name>
                     <user-device-name>40" Hisense Roku TV</user-device-name>
                     <power-mode>PowerOn</power-mode></device-info>"#),
        );
        // One more request in between — reachable is not the same as controllable, so the flow
        // asks for the channel list before it offers anything. Then it is added.
        let (step, state) = Roku.setup("roku.player", &state, &probed);
        assert!(matches!(step, SetupStep::Fetch { .. }), "the control check: {step:?}");

        let mut apps = Args::new();
        apps.insert("response".into(), json!(r#"<apps><app id="12">Netflix</app></apps>"#));
        let (step, _) = Roku.setup("roku.player", &state, &apps);
        let SetupStep::Done { devices, .. } = step else {
            panic!("added, not offered back to be confirmed: {step:?}");
        };
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].driver_id, "roku.tv", "the box said it has a screen");
        assert_eq!(devices[0].label, "40\" Hisense Roku TV");
    }

    /// A search that turned up several is a real question, and still gets asked.
    #[test]
    fn several_rokus_are_still_offered_as_a_list() {
        let seed = json!({
            "ssdp_candidates": [
                { "address": "192.168.1.157" },
                { "address": "192.168.1.158" },
            ],
        });
        let (step, _) = Roku.discover("roku.player", &seed, &Args::new());
        assert!(
            matches!(step, SetupStep::Fetch { ref request, .. } if request.url.contains("device-info"))
                || matches!(step, SetupStep::Pick { .. }),
            "two boxes is a choice: {step:?}",
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
        let calls = driver.on_command(&mut inst, TV, "set_input", &a);
        let [HostCall::Http(req), HostCall::Notify { proxy, name, .. }] = calls.as_slice() else {
            panic!("expected a launch and a notify from the screen, got {calls:?}");
        };
        assert_eq!(*proxy, TV);
        assert_eq!(name, "input_changed");
        // Device-relative: core resolves the address, the port and the scheme against the
        // project. The driver spelling out `http://<addr>:8060` was the third place 8060 was
        // written down, after the manifest and the `Port` property it already reads.
        assert_eq!(req.url, "/launch/tvinput.hdmi2");
    }

    /// Volume and power belong to the panel. Sent to the streamer they used to work anyway —
    /// same box, same port — which is exactly what made a Roku player look like it had a
    /// volume control.
    #[test]
    fn the_screens_keys_are_the_screens() {
        let calls = Roku.on_command(&mut tv(), TV, "volume_up", &Args::new());
        let [HostCall::Http(req)] = calls.as_slice() else {
            panic!("expected one keypress, got {calls:?}");
        };
        assert_eq!(req.url, "/keypress/VolumeUp");

        let calls = Roku.on_command(&mut tv(), MEDIA, "volume_up", &Args::new());
        assert!(
            matches!(calls.as_slice(), [HostCall::Log { level, .. }] if level == "warn"),
            "a streamer has no volume of its own — got {calls:?}",
        );
    }

    #[test]
    fn the_app_launcher_uses_rokus_home_key() {
        let calls = Roku.on_command(&mut tv(), MEDIA, "open_app_launcher", &Args::new());
        let [HostCall::Http(req)] = calls.as_slice() else {
            panic!("expected one keypress, got {calls:?}");
        };
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "/keypress/Home");
    }

    #[test]
    fn set_input_refuses_a_connection_no_roku_has() {
        let driver = Roku;
        let mut inst = tv();
        let mut a = Args::new();
        a.insert("connection".into(), json!(7u64));
        let calls = driver.on_command(&mut inst, TV, "set_input", &a);
        assert!(matches!(calls.as_slice(), [HostCall::Log { level, .. }] if level == "warn"));
    }

    /// A Roku TV lists its inputs among its channels, so the set itself says how many HDMI
    /// ports it has — which is the only place they come from, since neither manifest declares
    /// any.
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
        let Some(connections) = calls.iter().find_map(|c| match c {
            HostCall::Connections { connections } => Some(connections.clone()),
            _ => None,
        }) else {
            panic!("expected connections, got {calls:?}");
        };

        let ids: Vec<LocalId> = connections.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![1001, 1002, 1003, 1201]);
        assert!(!ids.contains(&1004), "this set has three HDMI ports");
        assert!(
            connections
                .iter()
                .all(|c| c.dir == Direction::Consumer && c.proxy == Some(TV as LocalId)),
            "the screen's jacks, not the streamer's"
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

    /// A Roku TV leads with its screen, and a Roku player has no screen to lead with.
    ///
    /// A product decision that lives entirely in the order and flags of a TOML file, so nothing
    /// about the code would break if somebody tidied the two proxies back into id order. What
    /// would break is the reading: a device somebody calls the living room television, filed in
    /// the tree under "Roku Home", with the screen underneath as an accessory of the app menu.
    /// Bindings are created in declaration order — see `Project::install` — and `primary` is
    /// what the assistant falls back to when a command could go to either.
    ///
    /// Read as text rather than parsed. `Manifest` lives behind the SDK's `contracts` feature,
    /// which a driver does not compile, and this is not worth being the first one that does:
    /// `junod validate-driver` already checks the manifest is *valid* on every release, and the
    /// only thing it cannot check is which of two valid orders somebody meant.
    #[test]
    fn a_roku_tv_leads_with_its_screen() {
        let tv = include_str!("../manifests/roku.tv.toml");
        let screen = tv.find(r#"type = "tv""#).expect("roku.tv declares a screen");
        let player = tv.find(r#"type = "media_player""#).expect("and the streamer in it");
        assert!(screen < player, "the television is the device; the streamer is inside it");
        assert!(
            tv[screen..player].contains("primary = true"),
            "and it is what the driver leads with",
        );
        assert!(
            !tv[player..].contains("primary = true"),
            "only one proxy may be primary, and it is not this one",
        );

        let stick = include_str!("../manifests/roku.player.toml");
        assert!(!stick.contains(r#"type = "tv""#), "a stick has no panel to offer");
    }

    #[test]
    fn ids_round_trip_between_the_channel_id_and_the_connection() {
        assert_eq!(connection_id("tvinput.hdmi3"), Some(1003));
        assert_eq!(tvinput_for(1003).as_deref(), Some("tvinput.hdmi3"));
        assert_eq!(connection_id("12"), None, "Netflix is not a jack");
    }
}

export_driver!(Roku);
