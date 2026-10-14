//! Embeds the attach-mode agent in the binary when `TRAFFIC_POLICE_EMBED_AGENT` names the
//! directory `./gradlew :attach-agent:agentArtifacts` wrote (`1`: the one in this source tree;
//! a relative path is taken from the repository root). Release builds set it, so the binary
//! needs nothing else to attach; without it, the agent is looked for at run time.
//!
//! Each file goes in gzip-compressed (flate2, the best level), and attach mode unpacks it when it
//! first needs the agent (main.rs): the 3.6 MB of agent take 1.3 MB of the binary. Without an
//! agent, stand-in files are packed the same way for the test that unpacking gives back every
//! file as it was.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;

/// In the order `embedded::AGENT` lists them (main.rs): the agent per ABI, then the dex files.
const FILES: [&str; 5] = [
    "arm64-v8a/libtrafficpolice_agent.so",
    "armeabi-v7a/libtrafficpolice_agent.so",
    "x86_64/libtrafficpolice_agent.so",
    "traffic-police-boot.dex",
    "traffic-police-runtime.dex",
];

fn main() {
    println!("cargo:rerun-if-env-changed=TRAFFIC_POLICE_EMBED_AGENT");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let code = match std::env::var("TRAFFIC_POLICE_EMBED_AGENT").ok().filter(|v| !v.is_empty() && v != "0") {
        None => {
            let dir = out.join("stand-in");
            std::fs::create_dir_all(&dir).expect("creating the stand-ins' directory");
            let files: Vec<PathBuf> = (0..FILES.len())
                .map(|k| {
                    let path = dir.join(k.to_string());
                    std::fs::write(&path, stand_in(k)).expect("writing a stand-in");
                    path
                })
                .collect();
            let packed = pack(&files, &dir);
            format!(
                "/// No agent was embedded at build time.\npub const AGENT: Option<[&[u8]; 5]> = None;\n{}",
                packed_for_test(&files, &packed)
            )
        }
        Some(value) => {
            let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR")).join("../..");
            let dir = match value.as_str() {
                "1" => root.join("android/attach-agent/build/outputs/agent"),
                v if PathBuf::from(v).is_absolute() => PathBuf::from(v),
                v => root.join(v),
            };
            let files: Vec<PathBuf> = FILES
                .iter()
                .map(|f| {
                    let path = dir.join(f);
                    println!("cargo:rerun-if-changed={}", path.display());
                    assert!(
                        path.is_file(),
                        "TRAFFIC_POLICE_EMBED_AGENT: {} is missing; build the agent first (cd android && ./gradlew :attach-agent:agentArtifacts)",
                        path.display()
                    );
                    path
                })
                .collect();
            let packed = pack(&files, &out.join("agent"));
            format!(
                "/// The agent embedded at build time, from {:?}, each file gzip-compressed.\npub const AGENT: Option<[&[u8]; 5]> = Some([{}]);\n{}",
                dir.display().to_string(),
                packed.join(", "),
                packed_for_test(&files, &packed)
            )
        }
    };
    std::fs::write(out.join("embedded_agent.rs"), code).expect("writing embedded_agent.rs");
}

/// Compresses each file into `dir` (`0.gz`, `1.gz`, ...); the `include_bytes!` of each.
fn pack(files: &[PathBuf], dir: &Path) -> Vec<String> {
    std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("creating {}: {e}", dir.display()));
    files
        .iter()
        .enumerate()
        .map(|(k, file)| {
            let bytes = std::fs::read(file).unwrap_or_else(|e| panic!("reading {}: {e}", file.display()));
            let mut gz = GzEncoder::new(Vec::new(), Compression::best());
            gz.write_all(&bytes).unwrap_or_else(|e| panic!("compressing {}: {e}", file.display()));
            let packed = gz.finish().unwrap_or_else(|e| panic!("compressing {}: {e}", file.display()));
            let path = dir.join(format!("{k}.gz"));
            std::fs::write(&path, packed).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
            format!("include_bytes!({:?})", path.display().to_string())
        })
        .collect()
}

/// For the test: each file as packed, with the file it was packed from.
fn packed_for_test(files: &[PathBuf], packed: &[String]) -> String {
    let pairs: Vec<String> =
        files.iter().zip(packed).map(|(file, bytes)| format!("({:?}, {bytes})", file.display().to_string())).collect();
    format!(
        "/// Each file packed as the agent is, and the file it was packed from.\n#[cfg(test)]\npub const PACKED: [(&str, &[u8]); 5] = [{}];\n",
        pairs.join(", ")
    )
}

/// A stand-in for the `k`th file when no agent is embedded, 17 bytes to 160 KB: runs of text,
/// which compress, and of noise, which does not.
fn stand_in(k: usize) -> Vec<u8> {
    let text = b"traffic-police stand-in ";
    let mut noise = 0x9E37_79B9_7F4A_7C15_u64 ^ k as u64;
    (0..k * 40_000 + 17)
        .map(|i| {
            if (i / 4096) % 2 == 0 {
                text[i % text.len()]
            } else {
                noise = noise.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                (noise >> 56) as u8
            }
        })
        .collect()
}
