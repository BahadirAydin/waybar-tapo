use base64::{Engine, engine::general_purpose::STANDARD};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    net::{IpAddr, UdpSocket},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
use tapo::{ApiClient, Error, HandlerExt, TapoResponseError};

const CONTROLS: &str = "Left: white / warm · Middle: warm\nRight: on/off · Scroll: brightness ±5%\nColor and brightness controls turn the LED on.";
const WARM_HUE: u16 = 20;
const WARM_SATURATION: u8 = 80;
const WARM_VALUE: u8 = 100;
// TP-Link TDP probe: version 2, op_code 1 (probe), empty payload. The last
// four bytes are the CRC32 of the header with 0x5a6b7c8d sitting in that slot,
// so the whole query is a constant and needs no key exchange. Devices reply to
// the source port with a 16-byte header followed by plaintext JSON.
const DISCOVERY_QUERY: [u8; 16] = [
    0x02, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46, 0x3c, 0xb5, 0xd3,
];
const DISCOVERY_PORT: u16 = 20002;
const DISCOVERY_WINDOW: Duration = Duration::from_secs(2);

#[derive(Deserialize, Serialize)]
struct Config {
    device_ip: String,
    username: String,
    password: String,
}
impl Config {
    fn read(path: &Path) -> Result<Self, &'static str> {
        let bytes =
            fs::read(path).map_err(|_| "Cannot read waybar/tapo.json. Run tapo-setup.sh first.")?;
        let cfg: Self = serde_json::from_slice(&bytes)
            .map_err(|_| "Invalid Tapo config; required: device_ip, username, password.")?;
        if [&cfg.device_ip, &cfg.username, &cfg.password]
            .iter()
            .any(|v| v.trim().is_empty())
        {
            return Err("Tapo connection settings cannot be empty.");
        }
        Ok(cfg)
    }
}

#[derive(Deserialize)]
struct Discovered {
    #[serde(default)]
    ip: String,
    device_model: String,
    #[serde(default)]
    mac: String,
}
impl Discovered {
    fn is_l900(&self) -> bool {
        self.device_model.starts_with("L900")
    }
}
fn parse_discovery(datagram: &[u8], src: IpAddr) -> Option<Discovered> {
    let value: Value = serde_json::from_slice(datagram.get(16..)?).ok()?;
    let mut found: Discovered = serde_json::from_value(value.get("result")?.clone()).ok()?;
    if found.ip.trim().is_empty() {
        found.ip = src.to_string();
    }
    Some(found)
}
/// Broadcast one probe and collect replies. This blocks, which is fine: the
/// runtime is single-threaded and nothing else is in flight while it runs.
fn discover(window: Duration, stop_at_strip: bool) -> io::Result<Vec<Discovered>> {
    let socket = UdpSocket::bind(("0.0.0.0", 0))?;
    socket.set_broadcast(true)?;
    socket.set_read_timeout(Some(Duration::from_millis(200)))?;
    socket.send_to(&DISCOVERY_QUERY, ("255.255.255.255", DISCOVERY_PORT))?;
    let deadline = Instant::now() + window;
    let mut found: Vec<Discovered> = Vec::new();
    let mut buf = [0; 2048];
    while Instant::now() < deadline {
        let (read, from) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => return Err(e),
        };
        let Some(device) = parse_discovery(&buf[..read], from.ip()) else {
            continue;
        };
        if found.iter().any(|d| d.ip == device.ip) {
            continue;
        }
        let strip = device.is_l900();
        found.push(device);
        // The strip answers in milliseconds; no reason to hold the whole window.
        if strip && stop_at_strip {
            break;
        }
    }
    Ok(found)
}
fn discover_l900(window: Duration) -> Option<Discovered> {
    discover(window, true)
        .ok()?
        .into_iter()
        .find(Discovered::is_l900)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Action {
    Status,
    Color,
    Warm,
    Toggle,
    Brighter,
    Dimmer,
}
impl Action {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "status" => Self::Status,
            "color" => Self::Color,
            "warm" => Self::Warm,
            "toggle" => Self::Toggle,
            "brighter" => Self::Brighter,
            "dimmer" => Self::Dimmer,
            _ => return None,
        })
    }
}

#[derive(Serialize)]
struct Output {
    text: String,
    class: &'static str,
    tooltip: String,
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn output(text: impl Into<String>, class: &'static str, detail: impl AsRef<str>) -> Output {
    Output {
        text: text.into(),
        class,
        tooltip: escape(&format!("{}\n\n{CONTROLS}", detail.as_ref())),
    }
}
fn failure(detail: &str) -> Output {
    output("⚠", "error", detail)
}
fn offline() -> Output {
    output(
        "",
        "offline",
        "Tapo unreachable or timed out on the local network.",
    )
}
fn emit(out: &Output) {
    println!(
        "{}",
        serde_json::to_string(out).expect("serializable status")
    );
}
fn xdg(var: &str, fallback: &str) -> PathBuf {
    env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(fallback))
}
fn config_path() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("waybar/tapo.json")
}

#[derive(Deserialize)]
struct State {
    device_on: bool,
    brightness: u8,
    model: String,
    nickname: String,
    hue: Option<u16>,
    saturation: Option<u16>,
    #[serde(default)]
    color_temp: u16,
    #[serde(default)]
    dynamic_light_effect_enable: bool,
    #[serde(default)]
    dynamic_light_effect_id: Option<String>,
    #[serde(default)]
    lighting_effect: Option<Value>,
}
impl State {
    fn color(&self) -> Option<(u16, u16)> {
        self.hue
            .zip(self.saturation)
            .filter(|&(h, s)| h <= 360 && s <= 100)
    }
    fn effect(&self) -> bool {
        self.dynamic_light_effect_enable
            || self
                .lighting_effect
                .as_ref()
                .is_some_and(|e| e["enable"] == 1 || e["enable"] == true)
    }
    fn describe(&self) -> Output {
        let name = STANDARD
            .decode(&self.nickname)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Tapo LED".into());
        let title = format!("{name} · {}", self.model);
        if !self.device_on {
            return output("○", "off", format!("{title}\nConnected · Off"));
        }
        let detail = format!("{title}\nConnected · On · {}%", self.brightness);
        if self.effect() {
            let id = self.dynamic_light_effect_id.as_deref().unwrap_or("active");
            return output(
                "✦",
                "effect",
                format!(
                    "{detail}\nEffect: {id}\nPause the effect in the Tapo app before adjusting color or brightness."
                ),
            );
        }
        if self.color_temp > 0 {
            return output(
                "●",
                "on",
                format!("{detail}\nColor temperature: {}K", self.color_temp),
            );
        }
        if let Some((h, s)) = self.color() {
            let color = color_hex(h, s);
            let label = if s == 0 {
                "White"
            } else if (h, s) == (WARM_HUE, u16::from(WARM_SATURATION)) {
                "Warm"
            } else {
                "Custom"
            };
            output(
                format!("<span foreground=\"{color}\">●</span>"),
                "on",
                format!(
                    "{detail}\nColor: near {label} · HSV({h}, {s}, {})",
                    self.brightness
                ),
            )
        } else {
            output("●", "on", format!("{detail}\nColor unavailable"))
        }
    }
}
fn color_hex(h: u16, s: u16) -> String {
    let h = f64::from(h % 360) / 60.;
    let s = f64::from(s.min(100)) / 100.;
    let x = s * (1. - (h % 2. - 1.).abs());
    let (r, g, b) = match h as u8 {
        0 => (s, x, 0.),
        1 => (x, s, 0.),
        2 => (0., s, x),
        3 => (0., x, s),
        4 => (x, 0., s),
        _ => (s, 0., x),
    };
    let channel = |c: f64| ((c + 1. - s) * 255.).round() as u8;
    format!("#{:02x}{:02x}{:02x}", channel(r), channel(g), channel(b))
}
fn payload(state: &State, action: Action) -> Result<Value, &'static str> {
    if action == Action::Toggle {
        return Ok(json!({"device_on": !state.device_on}));
    }
    if state.effect() {
        return Err(
            "Pause the active effect in the Tapo app before adjusting color or brightness.",
        );
    }
    match action {
        Action::Brighter | Action::Dimmer => {
            let delta = if action == Action::Brighter { 5 } else { -5 };
            Ok(
                json!({"device_on":true,"brightness":(i16::from(state.brightness)+delta).clamp(1,100)}),
            )
        }
        Action::Color | Action::Warm => {
            let is_warm = state.color_temp == 0
                && state.color() == Some((WARM_HUE, u16::from(WARM_SATURATION)));
            if action == Action::Warm || !is_warm {
                Ok(json!({"device_on":true,"brightness":WARM_VALUE,
                    "color_temp":0,"hue":WARM_HUE,"saturation":WARM_SATURATION}))
            } else {
                // RGB white is represented by zero saturation. The library's
                // builder rejects that value, but the authenticated API accepts it.
                Ok(
                    json!({"device_on":true,"brightness":state.brightness.clamp(1,100),
                    "color_temp":0,"hue":0,"saturation":0}),
                )
            }
        }
        _ => Err("Status does not change the device."),
    }
}
fn api_error(err: Error) -> Output {
    match err {
        Error::Http(e) if e.is_connect() || e.is_timeout() => offline(),
        Error::DeviceNotFound => offline(),
        Error::Tapo(TapoResponseError::Unauthorized { .. }) => {
            failure("Tapo authentication failed. Check the credentials in waybar/tapo.json.")
        }
        _ => failure("Tapo request failed. Check the device, credentials and local connection."),
    }
}
async fn request(cfg: &Config, action: Action) -> Result<Output, Error> {
    let device = ApiClient::new(cfg.username.clone(), cfg.password.clone())
        .with_timeout(Duration::from_secs(4))
        .l900(cfg.device_ip.clone())
        .await?;
    // `debug` exposes full device info, including optional effect fields. No
    // logger is installed; credentials and raw device data are never printed.
    let mut state: State = serde_json::from_value(device.get_device_info_json().await?)?;
    if !state.model.starts_with("L900") {
        return Ok(failure("This module is configured for an L900 LED strip."));
    }
    if action != Action::Status {
        let data = match payload(&state, action) {
            Ok(v) => v,
            Err(e) => return Ok(failure(e)),
        };
        device.get_client().await.set_device_info(data).await?;
        state = serde_json::from_value(device.get_device_info_json().await?)?;
    }
    Ok(state.describe())
}
async fn attempt(cfg: &Config, action: Action) -> Output {
    match tokio::time::timeout(Duration::from_secs(8), request(cfg, action)).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => api_error(e),
        Err(_) => offline(),
    }
}
async fn query(action: Action) -> Output {
    let path = config_path();
    let mut cfg = match Config::read(&path) {
        Ok(c) => c,
        Err(e) => return failure(e),
    };
    let first = attempt(&cfg, action).await;
    // A new DHCP lease leaves the stored address dead while the strip is still
    // on the network. Re-probe only on that failure, and only adopt an address
    // that differs from the one just tried.
    if first.class != "offline" {
        return first;
    }
    let Some(found) = discover_l900(DISCOVERY_WINDOW) else {
        return first;
    };
    if found.ip == cfg.device_ip {
        return first;
    }
    cfg.device_ip = found.ip;
    if let Err(e) = write_config(&path, &cfg) {
        return failure(e);
    }
    attempt(&cfg, action).await
}
fn acquire(action: Action) -> io::Result<Option<File>> {
    let dir = xdg("XDG_CACHE_HOME", ".cache").join("waybar-tapo");
    fs::create_dir_all(&dir)?;
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join("device.lock"))?;
    let deadline =
        Instant::now() + Duration::from_millis(if action == Action::Status { 12000 } else { 300 });
    loop {
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(Some(file)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
}
fn import_config(source: &Path) -> Result<(), &'static str> {
    let cfg = Config::read(source)?;
    let path = config_path();
    fs::create_dir_all(path.parent().unwrap())
        .map_err(|_| "Cannot create configuration directory.")?;
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            println!("Existing Tapo configuration retained.");
            return Ok(());
        }
        Err(_) => return Err("Cannot create private Tapo configuration."),
    };
    let data = serde_json::to_vec(&cfg).map_err(|_| "Cannot encode Tapo configuration.")?;
    file.write_all(&data)
        .map_err(|_| "Cannot write Tapo configuration.")?;
    println!("Private Tapo configuration installed.");
    Ok(())
}

fn write_config(path: &Path, cfg: &Config) -> Result<(), &'static str> {
    let data = serde_json::to_vec(cfg).map_err(|_| "Cannot encode Tapo configuration.")?;
    let temp = path.with_extension("json.new");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|_| "Cannot create temporary Tapo configuration.")?;
    file.write_all(&data)
        .and_then(|_| file.sync_all())
        .map_err(|_| "Cannot write Tapo configuration.")?;
    fs::rename(temp, path).map_err(|_| "Cannot replace Tapo configuration.")?;
    Ok(())
}

fn set_ip(value: &str) -> Result<(), &'static str> {
    value.parse::<IpAddr>().map_err(|_| "Invalid IP address.")?;
    let path = config_path();
    let mut cfg = Config::read(&path)?;
    cfg.device_ip = value.to_owned();
    write_config(&path, &cfg)?;
    println!("Tapo device address updated.");
    Ok(())
}
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().is_some_and(|v| v == "--import-config") && args.len() == 2 {
        if let Err(e) = import_config(Path::new(&args[1])) {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }
    if args.first().is_some_and(|v| v == "--set-ip") && args.len() == 2 {
        if let Err(e) = set_ip(&args[1]) {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }
    if args.first().is_some_and(|v| v == "--discover") && args.len() == 1 {
        match discover(DISCOVERY_WINDOW, false) {
            Ok(found) if found.is_empty() => println!("No Tapo devices responded."),
            Ok(found) => {
                for d in found {
                    println!("{:15}  {:12}  {}", d.ip, d.device_model, d.mac);
                }
            }
            Err(_) => {
                eprintln!("Cannot send the discovery probe.");
                std::process::exit(1);
            }
        }
        return;
    }
    if args.first().is_some_and(|v| v == "--help") {
        println!(
            "waybar-tapo [status|color|warm|toggle|brighter|dimmer]\nwaybar-tapo --discover\nwaybar-tapo --import-config PATH\nwaybar-tapo --set-ip ADDRESS"
        );
        return;
    }
    let Some(action) = Action::parse(args.first().map(String::as_str).unwrap_or("status"))
        .filter(|_| args.len() <= 1)
    else {
        eprintln!("Unknown action. Use --help.");
        std::process::exit(2);
    };
    let lock = match acquire(action) {
        Ok(Some(f)) => f,
        Ok(None) => {
            if action == Action::Status {
                emit(&output("…", "busy", "Tapo is processing another request."));
            }
            return;
        }
        Err(_) => {
            emit(&failure("Cannot open Tapo request lock."));
            return;
        }
    };
    let out = query(action).await;
    drop(lock);
    if action == Action::Status {
        emit(&out);
    } else {
        if out.class == "error" || out.class == "offline" {
            let _ = Command::new("notify-send")
                .args([
                    "Tapo control failed",
                    out.tooltip.split("\n\n").next().unwrap_or("Request failed"),
                ])
                .status();
        }
        let _ = Command::new("pkill")
            .args(["-RTMIN+10", "-x", "waybar"])
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state() -> State {
        serde_json::from_value(json!({"device_on":true,"brightness":50,"model":"L900",
        "nickname":"TEVEIDxob21lPiY=","hue":240,"saturation":100}))
        .unwrap()
    }
    #[test]
    fn render_states_and_escape() {
        let mut s = state();
        let out = s.describe();
        assert!(out.text.contains("#0000ff"));
        assert!(out.tooltip.contains("LED &lt;home&gt;&amp;"));
        s.color_temp = 0;
        s.brightness = WARM_VALUE;
        s.hue = Some(WARM_HUE);
        s.saturation = Some(u16::from(WARM_SATURATION));
        let out = s.describe();
        assert!(out.text.contains("#ff7733"));
        assert!(out.tooltip.contains("near Warm"));
        s.device_on = false;
        assert_eq!(s.describe().class, "off");
        s.device_on = true;
        s.dynamic_light_effect_enable = true;
        assert_eq!(s.describe().class, "effect");
        s.dynamic_light_effect_enable = false;
        s.hue = None;
        assert!(s.describe().tooltip.contains("Color unavailable"));
    }
    #[test]
    fn color_math() {
        assert_eq!(color_hex(0, 100), "#ff0000");
        assert_eq!(color_hex(120, 100), "#00ff00");
        assert_eq!(color_hex(240, 100), "#0000ff");
        assert_eq!(color_hex(360, 100), "#ff0000");
        assert_eq!(color_hex(240, 0), "#ffffff");
    }
    #[test]
    fn actions_preserve_brightness_and_white() {
        let mut s = state();
        let p = payload(&s, Action::Color).unwrap();
        assert_eq!(p["hue"], WARM_HUE);
        assert_eq!(p["saturation"], WARM_SATURATION);
        assert_eq!(p["brightness"], WARM_VALUE);
        assert_eq!(payload(&s, Action::Warm).unwrap()["brightness"], WARM_VALUE);
        s.color_temp = 0;
        s.hue = Some(WARM_HUE);
        s.saturation = Some(u16::from(WARM_SATURATION));
        let p = payload(&s, Action::Color).unwrap();
        assert_eq!(p["saturation"], 0);
        assert_eq!(p["color_temp"], 0);
        assert_eq!(p["hue"], 0);
        s.color_temp = 0;
        s.hue = Some(0);
        s.saturation = Some(0);
        let p = payload(&s, Action::Color).unwrap();
        assert_eq!(p["hue"], WARM_HUE);
        assert_eq!(p["saturation"], WARM_SATURATION);
        assert_eq!(p["brightness"], WARM_VALUE);
    }
    #[test]
    fn bounds_power_and_effect_guard() {
        let mut s = state();
        s.brightness = 99;
        assert_eq!(payload(&s, Action::Brighter).unwrap()["brightness"], 100);
        s.brightness = 1;
        s.device_on = false;
        let p = payload(&s, Action::Dimmer).unwrap();
        assert_eq!(p["brightness"], 1);
        assert_eq!(p["device_on"], true);
        assert_eq!(payload(&s, Action::Toggle).unwrap()["device_on"], true);
        s.dynamic_light_effect_enable = true;
        assert!(payload(&s, Action::Color).is_err());
        assert!(payload(&s, Action::Toggle).is_ok());
        assert!(payload(&s, Action::Status).is_err());
    }
    #[test]
    fn errors_do_not_expose_secrets() {
        let out = api_error(Error::Tapo(TapoResponseError::Unauthorized {
            kind: "test",
            description: "secret".into(),
        }));
        assert_eq!(out.class, "error");
        assert!(!out.tooltip.contains("secret"));
    }

    fn datagram(body: &[u8]) -> Vec<u8> {
        let mut packet = vec![0; 16];
        packet.extend_from_slice(body);
        packet
    }
    #[test]
    fn discovery_query_is_the_documented_probe() {
        // Version 2, op_code 1, then the fixed CRC32 over the placeholder header.
        assert_eq!(&DISCOVERY_QUERY[..4], &[0x02, 0x00, 0x00, 0x01]);
        assert_eq!(&DISCOVERY_QUERY[12..], &[0x46, 0x3c, 0xb5, 0xd3]);
    }
    #[test]
    fn parses_reply_and_only_adopts_the_strip() {
        let packet = datagram(
            br#"{"result":{"device_id":"abc","device_type":"SMART.TAPOBULB",
            "device_model":"L900-5(EU)","ip":"192.168.1.3","mac":"54-AF-97-B1-F5-53"},
            "error_code":0}"#,
        );
        let found = parse_discovery(&packet, "192.168.1.3".parse().unwrap()).unwrap();
        assert_eq!(found.ip, "192.168.1.3");
        assert_eq!(found.mac, "54-AF-97-B1-F5-53");
        assert!(found.is_l900());
        // Another Tapo device on the same network must not be adopted.
        let packet = datagram(br#"{"result":{"device_model":"P110(EU)","ip":"192.168.1.8"}}"#);
        let other = parse_discovery(&packet, "192.168.1.8".parse().unwrap()).unwrap();
        assert!(!other.is_l900());
    }
    #[test]
    fn discovery_rejects_short_and_malformed_datagrams() {
        let src: IpAddr = "192.168.1.3".parse().unwrap();
        assert!(parse_discovery(&[0; 8], src).is_none());
        assert!(parse_discovery(&[0; 16], src).is_none());
        assert!(parse_discovery(&datagram(b"not json"), src).is_none());
        assert!(parse_discovery(&datagram(br#"{"error_code":-1}"#), src).is_none());
    }
    #[test]
    fn discovery_falls_back_to_the_source_address() {
        let packet = datagram(br#"{"result":{"device_model":"L900-5(EU)","ip":""}}"#);
        let found = parse_discovery(&packet, "192.168.1.77".parse().unwrap()).unwrap();
        assert_eq!(found.ip, "192.168.1.77");
    }

    #[test]
    fn set_ip_rejects_invalid_addresses_before_reading_config() {
        assert_eq!(set_ip("not-an-address"), Err("Invalid IP address."));
    }
}
