//! Debuggable processes from `track-app` (Android 12+) or `track-jdwp` (research 04 §6).

use crate::pb;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppProcess {
    pub pid: u32,
    pub debuggable: bool,
    pub profileable: bool,
    /// ISA name, e.g. `arm64`.
    pub architecture: Option<String>,
    /// Present with the `app_info` feature (Android 15+ adbd).
    pub process_name: Option<String>,
    pub package_names: Vec<String>,
    pub uid: Option<u32>,
}

/// An `AppProcesses` message.
pub fn parse_app_processes(buf: &[u8]) -> Option<Vec<AppProcess>> {
    let mut out = Vec::new();
    for (field, value) in pb::fields(buf)? {
        if field != 1 {
            continue;
        }
        let mut p = AppProcess {
            pid: 0,
            debuggable: false,
            profileable: false,
            architecture: None,
            process_name: None,
            package_names: Vec::new(),
            uid: None,
        };
        for (f, v) in pb::fields(value.as_bytes()?)? {
            match f {
                1 => p.pid = u32::try_from(v.as_u64()?).ok()?,
                2 => p.debuggable = v.as_u64()? != 0,
                3 => p.profileable = v.as_u64()? != 0,
                4 => p.architecture = v.as_str().map(str::to_string),
                6 => p.process_name = v.as_str().map(str::to_string),
                7 => p.package_names.push(v.as_str()?.to_string()),
                9 => p.uid = v.as_u64().and_then(|u| u32::try_from(u).ok()),
                _ => {}
            }
        }
        out.push(p);
    }
    Some(out)
}

/// A `track-jdwp` message: one pid per line.
pub fn parse_jdwp(text: &str) -> Vec<u32> {
    text.lines().filter_map(|l| l.trim().parse().ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_process_entries() {
        // pid 4312, debuggable, arch "arm64", process name, one package
        let mut entry = vec![0x08, 0xd8, 0x21, 0x10, 0x01, 0x22, 0x05];
        entry.extend_from_slice(b"arm64");
        entry.extend_from_slice(&[0x32, 0x03]);
        entry.extend_from_slice(b"app");
        entry.extend_from_slice(&[0x3a, 0x07]);
        entry.extend_from_slice(b"com.app");
        let mut msg = vec![0x0a, entry.len() as u8];
        msg.extend_from_slice(&entry);
        let p = parse_app_processes(&msg).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].pid, 4312);
        assert!(p[0].debuggable);
        assert_eq!(p[0].architecture.as_deref(), Some("arm64"));
        assert_eq!(p[0].process_name.as_deref(), Some("app"));
        assert_eq!(p[0].package_names, vec!["com.app"]);
        assert_eq!(parse_jdwp("123\n456\n"), vec![123, 456]);
    }
}
