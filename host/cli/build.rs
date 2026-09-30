//! Embeds the attach-mode agent in the binary when `TRAFFIC_POLICE_EMBED_AGENT` names the
//! directory `./gradlew :attach-agent:agentArtifacts` wrote (`1`: the one in this source tree;
//! a relative path is taken from the repository root). Release builds set it, so the binary
//! needs nothing else to attach; without it, the agent is looked for at run time.

use std::path::PathBuf;

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
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("embedded_agent.rs");
    let code = match std::env::var("TRAFFIC_POLICE_EMBED_AGENT").ok().filter(|v| !v.is_empty() && v != "0") {
        None => "/// No agent was embedded at build time.\npub const AGENT: Option<[&[u8]; 5]> = None;\n".to_string(),
        Some(value) => {
            let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR")).join("../..");
            let dir = match value.as_str() {
                "1" => root.join("android/attach-agent/build/outputs/agent"),
                v if PathBuf::from(v).is_absolute() => PathBuf::from(v),
                v => root.join(v),
            };
            let files: Vec<String> = FILES
                .iter()
                .map(|f| {
                    let path = dir.join(f);
                    println!("cargo:rerun-if-changed={}", path.display());
                    assert!(
                        path.is_file(),
                        "TRAFFIC_POLICE_EMBED_AGENT: {} is missing; build the agent first (cd android && ./gradlew :attach-agent:agentArtifacts)",
                        path.display()
                    );
                    format!("include_bytes!({:?})", path.display().to_string())
                })
                .collect();
            format!(
                "/// The agent embedded at build time, from {:?}.\npub const AGENT: Option<[&[u8]; 5]> = Some([{}]);\n",
                dir.display().to_string(),
                files.join(", ")
            )
        }
    };
    std::fs::write(&out, code).expect("writing embedded_agent.rs");
}
