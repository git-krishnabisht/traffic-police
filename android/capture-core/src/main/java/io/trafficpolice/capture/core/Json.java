package io.trafficpolice.capture.core;

import java.nio.charset.Charset;

/**
 * A small streaming JSON writer (PROTOCOL.md §4). Integers are written exactly; strings are
 * escaped per RFC 8259. Used on the writer thread only.
 */
final class Json {
    static final Charset UTF_8 = Charset.forName("UTF-8");

    private final StringBuilder sb;
    private boolean comma;

    Json() {
        this(256);
    }

    Json(int capacity) {
        sb = new StringBuilder(capacity);
    }

    private void separate() {
        if (comma) {
            sb.append(',');
        }
    }

    Json obj() {
        separate();
        sb.append('{');
        comma = false;
        return this;
    }

    Json endObj() {
        sb.append('}');
        comma = true;
        return this;
    }

    Json arr() {
        separate();
        sb.append('[');
        comma = false;
        return this;
    }

    Json endArr() {
        sb.append(']');
        comma = true;
        return this;
    }

    Json key(String name) {
        separate();
        quote(name);
        sb.append(':');
        comma = false;
        return this;
    }

    Json str(String value) {
        separate();
        if (value == null) {
            sb.append("null");
        } else {
            quote(value);
        }
        comma = true;
        return this;
    }

    Json num(long value) {
        separate();
        sb.append(value);
        comma = true;
        return this;
    }

    Json bool(boolean value) {
        separate();
        sb.append(value ? "true" : "false");
        comma = true;
        return this;
    }

    Json nul() {
        separate();
        sb.append("null");
        comma = true;
        return this;
    }

    Json kv(String name, String value) {
        return key(name).str(value);
    }

    Json kv(String name, long value) {
        return key(name).num(value);
    }

    Json kv(String name, boolean value) {
        return key(name).bool(value);
    }

    /** Headers as `[["Name","value"], …]`, from a flat name/value array. */
    Json headers(String name, String[] pairs) {
        key(name).arr();
        if (pairs != null) {
            for (int i = 0; i + 1 < pairs.length; i += 2) {
                arr().str(pairs[i]).str(pairs[i + 1]).endArr();
            }
        }
        return endArr();
    }

    private void quote(String s) {
        sb.append('"');
        for (int i = 0, n = s.length(); i < n; i++) {
            char c = s.charAt(i);
            switch (c) {
                case '"':
                    sb.append("\\\"");
                    break;
                case '\\':
                    sb.append("\\\\");
                    break;
                case '\n':
                    sb.append("\\n");
                    break;
                case '\r':
                    sb.append("\\r");
                    break;
                case '\t':
                    sb.append("\\t");
                    break;
                case '\b':
                    sb.append("\\b");
                    break;
                case '\f':
                    sb.append("\\f");
                    break;
                default:
                    if (c < 0x20) {
                        sb.append("\\u00");
                        sb.append(HEX[c >> 4]);
                        sb.append(HEX[c & 0xf]);
                    } else {
                        sb.append(c);
                    }
            }
        }
        sb.append('"');
    }

    private static final char[] HEX = "0123456789abcdef".toCharArray();

    byte[] toBytes() {
        return sb.toString().getBytes(UTF_8);
    }

    @Override
    public String toString() {
        return sb.toString();
    }
}
