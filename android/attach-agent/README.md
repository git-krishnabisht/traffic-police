# Android attach agent

This module builds the JVMTI shared library and the dex files loaded by it. The native agent
transforms the four return hooks described in `docs/ARCHITECTURE.md §4.7.3`; the boot dex provides
the `java.lang` trampoline; the runtime dex contains the existing capture runtime plus its Android
entry point.

Build the device artifacts with:

```sh
cd android
./gradlew :attach-agent:agentArtifacts
```

The result is in `attach-agent/build/outputs/agent/`:

```text
arm64-v8a/libtrafficpolice_agent.so
armeabi-v7a/libtrafficpolice_agent.so
x86_64/libtrafficpolice_agent.so
traffic-police-boot.dex
traffic-police-runtime.dex
```

The native targets use NDK r28.2 and explicitly request 16 KB ELF segment alignment for all three
ABIs, including `armeabi-v7a`. Slicer and the JVMTI declaration are pinned under `third_party/`.

The host (`traffic-police --mode attach`) finds this directory in the source tree it was built
from, or takes `--agent-dir`; `TRAFFIC_POLICE_EMBED_AGENT=1 cargo build --release` in `host/`
builds the files into the binary, as the release workflow does. How the host installs and
attaches them is in `docs/ARCHITECTURE.md` §4.7.6.
