package io.trafficpolice.capture.core;

import java.security.MessageDigest;
import java.security.cert.Certificate;
import java.security.cert.X509Certificate;
import java.util.ArrayList;
import java.util.Collection;
import java.util.List;

/** Connection details (PROTOCOL.md §7.1 {@code Conn}). Built once per connection and reused. */
public final class ConnInfo {
    final String id;
    final boolean reused;
    final String protocol;
    final String remoteIp;
    final int remotePort;
    final String proxy;
    final String tlsVersion;
    final String cipher;
    final List<Cert> peer;

    public ConnInfo(String id, boolean reused, String protocol, String remoteIp, int remotePort, String proxy,
            String tlsVersion, String cipher, List<Cert> peer) {
        this.id = id;
        this.reused = reused;
        this.protocol = protocol;
        this.remoteIp = remoteIp;
        this.remotePort = remotePort;
        this.proxy = proxy;
        this.tlsVersion = tlsVersion;
        this.cipher = cipher;
        this.peer = peer;
    }

    /** The same connection seen again by a later exchange. */
    public ConnInfo asReused() {
        return reused ? this : new ConnInfo(id, true, protocol, remoteIp, remotePort, proxy, tlsVersion, cipher, peer);
    }

    void write(Json j) {
        j.obj();
        if (id != null) {
            j.kv("id", id);
        }
        j.kv("reused", reused);
        if (protocol != null) {
            j.kv("protocol", protocol);
        }
        if (remoteIp != null) {
            j.key("remote").obj().kv("ip", remoteIp).kv("port", remotePort).endObj();
        }
        if (proxy != null) {
            j.kv("proxy", proxy);
        }
        if (tlsVersion != null || cipher != null || (peer != null && !peer.isEmpty())) {
            j.key("tls").obj();
            if (tlsVersion != null) {
                j.kv("version", tlsVersion);
            }
            if (cipher != null) {
                j.kv("cipher", cipher);
            }
            j.key("peer").arr();
            if (peer != null) {
                for (Cert c : peer) {
                    c.write(j);
                }
            }
            j.endArr().endObj();
        }
        j.endObj();
    }

    /** A peer certificate summary. */
    public static final class Cert {
        final String subject;
        final String issuer;
        final long notBeforeMs;
        final long notAfterMs;
        final String sha256;
        final List<String> san;

        Cert(String subject, String issuer, long notBeforeMs, long notAfterMs, String sha256, List<String> san) {
            this.subject = subject;
            this.issuer = issuer;
            this.notBeforeMs = notBeforeMs;
            this.notAfterMs = notAfterMs;
            this.sha256 = sha256;
            this.san = san;
        }

        void write(Json j) {
            j.obj().kv("subject", subject).kv("issuer", issuer).kv("not_before_ms", notBeforeMs)
                    .kv("not_after_ms", notAfterMs);
            if (sha256 != null) {
                j.kv("sha256", sha256);
            }
            j.key("san").arr();
            for (String s : san) {
                j.str(s);
            }
            j.endArr().endObj();
        }
    }

    /** Summaries of up to four certificates, leaf first. Never throws. */
    public static List<Cert> summarize(List<Certificate> chain) {
        List<Cert> out = new ArrayList<>();
        if (chain == null) {
            return out;
        }
        for (Certificate c : chain) {
            if (out.size() == 4) {
                break;
            }
            if (!(c instanceof X509Certificate)) {
                continue;
            }
            X509Certificate x = (X509Certificate) c;
            try {
                out.add(new Cert(x.getSubjectX500Principal().getName(), x.getIssuerX500Principal().getName(),
                        x.getNotBefore().getTime(), x.getNotAfter().getTime(), sha256(x), sans(x)));
            } catch (RuntimeException ignored) {
                // a malformed certificate is simply not summarized
            }
        }
        return out;
    }

    private static String sha256(X509Certificate x) {
        try {
            byte[] d = MessageDigest.getInstance("SHA-256").digest(x.getEncoded());
            StringBuilder sb = new StringBuilder(64);
            for (byte b : d) {
                sb.append(Character.forDigit((b >> 4) & 0xf, 16)).append(Character.forDigit(b & 0xf, 16));
            }
            return sb.toString();
        } catch (Exception e) {
            return null;
        }
    }

    private static List<String> sans(X509Certificate x) {
        List<String> out = new ArrayList<>();
        try {
            Collection<List<?>> names = x.getSubjectAlternativeNames();
            if (names != null) {
                for (List<?> n : names) {
                    // 2 = dNSName, 7 = iPAddress
                    if (n.size() >= 2 && n.get(0) instanceof Integer && n.get(1) instanceof String) {
                        int type = (Integer) n.get(0);
                        if (type == 2 || type == 7) {
                            out.add((String) n.get(1));
                        }
                    }
                }
            }
        } catch (Exception ignored) {
            // leave the list empty
        }
        return out;
    }
}
