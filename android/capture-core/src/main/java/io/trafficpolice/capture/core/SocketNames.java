package io.trafficpolice.capture.core;

import java.util.Locale;
import java.util.zip.CRC32;

/** The runtime's abstract socket name (PROTOCOL.md §2); the host computes the same function. */
public final class SocketNames {
    public static final String PREFIX = "traffic-police_";
    /** Abstract names are limited to 107 bytes; prefix, separator and a 7-digit pid take 23. */
    static final int MAX_PACKAGE = 84;

    private SocketNames() {}

    public static String forProcess(String packageName, int pid) {
        return PREFIX + packagePart(packageName) + "_" + pid;
    }

    static String packagePart(String packageName) {
        if (packageName.length() <= MAX_PACKAGE) {
            return packageName;
        }
        CRC32 crc = new CRC32();
        crc.update(packageName.getBytes(Json.UTF_8));
        return packageName.substring(0, 75) + "~" + String.format(Locale.ROOT, "%08x", crc.getValue());
    }
}
