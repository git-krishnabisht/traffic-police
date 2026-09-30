# Vendored native dependencies

## dexter/slicer

- Upstream: Android Open Source Project, `platform/tools/dexter`
- Pinned revision: `d992a222`
- Imported directory: `slicer/`
- License: Apache-2.0; source files retain their AOSP notices. The full license is in
  `SLICER_LICENSE`.
- Local source snapshot used for this import: `scratchpad/vendor/dexter/slicer`.

## jvmti.h

- Upstream: Android Open Source Project ART, `openjdkjvmti/include/jvmti.h`
- Pinned blob: `de07c163`
- SHA-1 of imported header: `5908104ab2b96f500bc8c37a0c6728cac985db9e`
- License: GPL-2.0-only with the Classpath exception in `GPL-2.0.txt`; the AOSP notice is in
  `JVMTI_HEADER_NOTICE`, and the JDK distribution exception is in `OPENJDK_ASSEMBLY_EXCEPTION.txt`.
- This is an API declaration header. The agent links against the Android runtime's JVMTI
  implementation and does not distribute an implementation of JVMTI.
