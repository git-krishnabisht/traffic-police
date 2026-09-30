//! Devices as the adb server reports them (docs/research/04-adb-protocol.md §2).

use crate::pb;

pub type TransportId = u64;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Device {
    pub serial: String,
    /// `device`, `offline`, `unauthorized`, `authorizing`, `connecting`, `no permissions`, …
    pub state: String,
    pub product: Option<String>,
    pub model: Option<String>,
    pub device: Option<String>,
    pub transport_id: TransportId,
}

impl Device {
    /// Only `device` (and the recovery-like states) accept services; we need `device`.
    pub fn is_online(&self) -> bool {
        self.state == "device"
    }

    /// "Pixel 8 [emulator-5554]" style label; the model with underscores as spaces.
    pub fn label(&self) -> String {
        match &self.model {
            Some(m) => format!("{} [{}]", m.replace('_', " "), self.serial),
            None => self.serial.clone(),
        }
    }
}

/// `adb.proto.ConnectionState` (adb_host.proto:25-45).
fn state_name(v: u64) -> &'static str {
    match v {
        1 => "connecting",
        2 => "authorizing",
        3 => "unauthorized",
        4 => "no permissions",
        5 => "detached",
        6 => "offline",
        7 => "bootloader",
        8 => "device",
        9 => "host",
        10 => "recovery",
        11 => "sideload",
        12 => "rescue",
        _ => "unknown",
    }
}

/// A `Devices` message from `host:track-devices-proto-binary`.
pub fn parse_proto(buf: &[u8]) -> Option<Vec<Device>> {
    let mut out = Vec::new();
    for (field, value) in pb::fields(buf)? {
        if field != 1 {
            continue;
        }
        let mut d = Device {
            serial: String::new(),
            state: "unknown".into(),
            product: None,
            model: None,
            device: None,
            transport_id: 0,
        };
        for (f, v) in pb::fields(value.as_bytes()?)? {
            match f {
                1 => d.serial = v.as_str()?.to_string(),
                2 => d.state = state_name(v.as_u64()?).to_string(),
                4 => d.product = v.as_str().map(str::to_string),
                5 => d.model = v.as_str().map(str::to_string),
                6 => d.device = v.as_str().map(str::to_string),
                10 => d.transport_id = v.as_u64()?,
                _ => {}
            }
        }
        out.push(d);
    }
    Some(out)
}

/// Lines of `host:devices-l` / `host:track-devices-l`:
/// `<serial padded to 22> <state> [devpath] [product:x] [model:x] [device:x] transport_id:<n>`.
pub fn parse_long(text: &str) -> Vec<Device> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        let Some((serial, rest)) = line.split_once(char::is_whitespace) else { continue };
        let rest = rest.trim_start();
        let mut tokens: Vec<&str> = rest.split_whitespace().collect();
        // the transport id is always last ("so that anyone parsing ... can find it by scanning backwards")
        let Some(tid) = tokens.pop().and_then(|t| t.strip_prefix("transport_id:")).and_then(|t| t.parse().ok()) else {
            continue;
        };
        let mut d = Device {
            serial: serial.to_string(),
            state: String::new(),
            product: None,
            model: None,
            device: None,
            transport_id: tid,
        };
        let mut state_words = Vec::new();
        for t in tokens {
            if let Some(v) = t.strip_prefix("product:") {
                d.product = Some(v.to_string());
            } else if let Some(v) = t.strip_prefix("model:") {
                d.model = Some(v.to_string());
            } else if let Some(v) = t.strip_prefix("device:") {
                d.device = Some(v.to_string());
            } else if t.starts_with("usb:") || t.contains('/') {
                // devpath
            } else if d.product.is_none() && d.model.is_none() {
                state_words.push(t);
            }
        }
        // "no permissions (...); see [...]" spans several words
        d.state = if state_words.first() == Some(&"no") { "no permissions".into() } else { state_words.join(" ") };
        out.push(d);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_binary_tracker_message() {
        // captured from adb 37.0.0 (docs/research/04-adb-protocol.md §2): one unauthorized USB device
        let msg = b"\n#\n\x0f0011664BC002435\x10\x03\x1a\x07usb:2-18\x01@\xe0\x03P\x0c";
        let d = parse_proto(msg).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].serial, "0011664BC002435");
        assert_eq!(d[0].state, "unauthorized");
        assert_eq!(d[0].transport_id, 12);
        assert!(!d[0].is_online());
    }

    #[test]
    fn parses_the_long_text_format() {
        let text = "RZGL109WCKE            device usb:2-1 product:m36xins model:SM_M366B device:m36x transport_id:15\n\
                    emulator-5580          device product:sdk_gphone16k_arm64 model:sdk_gphone16k_arm64 device:emu64a16k transport_id:14\n\
                    0011664BC002435        unauthorized usb:2-1 transport_id:12\n";
        let d = parse_long(text);
        assert_eq!(d.len(), 3);
        assert_eq!(d[0].model.as_deref(), Some("SM_M366B"));
        assert_eq!(d[0].label(), "SM M366B [RZGL109WCKE]");
        assert_eq!(d[1].transport_id, 14);
        assert!(d[1].is_online());
        assert_eq!(d[2].state, "unauthorized");
    }
}
