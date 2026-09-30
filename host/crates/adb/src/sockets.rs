//! The capture runtime's abstract sockets: the name function (PROTOCOL.md §2, shared with the
//! device) and discovery through `/proc/net/unix` (research 04 §10).

pub const PREFIX: &str = "traffic-police_";
const MAX_PACKAGE: usize = 84;

/// `traffic-police_<package>_<pid>`, with long package names shortened to fit 107 bytes.
pub fn socket_name(package: &str, pid: u32) -> String {
    format!("{PREFIX}{}_{pid}", package_part(package))
}

/// The package as it appears in socket names: itself, or its first 75 characters, `~`, and the
/// CRC-32 of the whole name in 8 lowercase hex digits.
pub fn package_part(package: &str) -> String {
    if package.len() <= MAX_PACKAGE {
        return package.to_string();
    }
    let head: String = package.chars().take(75).collect();
    format!("{head}~{:08x}", crc32(package.as_bytes()))
}

/// CRC-32 (IEEE 802.3, as `java.util.zip.CRC32`).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// A listening capture runtime socket.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RuntimeSocket {
    /// The abstract name without the leading `@`.
    pub name: String,
    /// The package as encoded in the name (see [`package_part`]).
    pub package_part: String,
    pub pid: u32,
}

impl RuntimeSocket {
    pub fn is_for(&self, package: &str) -> bool {
        self.package_part == package_part(package)
    }
}

/// Listening `@traffic-police_*` sockets in `/proc/net/unix` output. Only listening rows count
/// (flags `00010000`, state `01`): accepted connections inherit the listener's name.
pub fn parse_proc_net_unix(text: &str) -> Vec<RuntimeSocket> {
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 8 || f[3] != "00010000" || f[5] != "01" {
            continue;
        }
        let Some(name) = f[7].strip_prefix('@') else { continue };
        let Some(rest) = name.strip_prefix(PREFIX) else { continue };
        // the pid follows the last '_' (package names may contain '_')
        let Some((package_part, pid)) = rest.rsplit_once('_') else { continue };
        let Ok(pid) = pid.parse() else { continue };
        let socket = RuntimeSocket { name: name.to_string(), package_part: package_part.to_string(), pid };
        if !out.contains(&socket) {
            out.push(socket);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_java() {
        // java.util.zip.CRC32 of "123456789" is 0xcbf43926 (the standard check value)
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn names_fit_and_match_the_device() {
        assert_eq!(socket_name("com.example", 4312), "traffic-police_com.example_4312");
        let long = format!("com.example{}", ".segment".repeat(12));
        let name = socket_name(&long, 1_234_567);
        assert!(name.len() <= 107, "{}", name.len());
        assert!(name.contains('~'));
    }

    #[test]
    fn finds_listening_runtime_sockets_only() {
        // lines as read from an API 37 emulator, plus an accepted connection and noise
        let text = "Num       RefCount Protocol Flags    Type St Inode Path\n\
            0000000000000000: 00000002 00000000 00010000 0001 01 26512 @traffic-police_io.trafficpolice.sample_4816\n\
            0000000000000000: 00000003 00000000 00000000 0001 03 26600 @traffic-police_io.trafficpolice.sample_4816\n\
            0000000000000000: 00000002 00000000 00010000 0001 01 26513 @traffic-police_my_app.with_underscores_77\n\
            0000000000000000: 00000002 00000000 00010000 0001 01 26514 @webview_devtools_remote_4816\n\
            0000000000000000: 00000002 00000000 00010000 0001 01 26515 /dev/socket/zygote\n";
        let s = parse_proc_net_unix(text);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].pid, 4816);
        assert!(s[0].is_for("io.trafficpolice.sample"));
        assert_eq!(s[1].package_part, "my_app.with_underscores");
        assert_eq!(s[1].pid, 77);
    }
}
