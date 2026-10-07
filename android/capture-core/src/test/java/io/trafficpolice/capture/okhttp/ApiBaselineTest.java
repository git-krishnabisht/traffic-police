package io.trafficpolice.capture.okhttp;

import static org.junit.Assert.assertTrue;
import static org.junit.Assume.assumeTrue;

import java.io.ByteArrayInputStream;
import java.io.DataInputStream;
import java.io.File;
import java.io.IOException;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.nio.file.Files;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Deque;
import java.util.HashSet;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Set;
import java.util.TreeSet;
import org.junit.Test;

/**
 * The compiled runtime references only what OkHttp 3.9.0 and Okio 1.13.0 have (ARCHITECTURE.md
 * §4.1): the adapter is compiled against OkHttp 3.14.9 (and ForwardingEventListener against
 * 5.5.0), so javac alone would let a 3.10+ member through, and it would fail with
 * {@code NoSuchMethodError} only when an app with an older OkHttp reached that line. Every class,
 * method and field reference to {@code okhttp3} and {@code okio} in the class files is looked up
 * in the OkHttp 3.9.0 suite's classpath. The few that are newer on purpose are listed in
 * {@link #GUARDED} with what keeps them from running on an older OkHttp; a guard that is no
 * longer needed fails the test too, so the list stays exact.
 */
public class ApiBaselineTest {
    /** References to members newer than OkHttp 3.9.0, each with the reason it never runs there. */
    private static final Set<String> GUARDED = new HashSet<>(Arrays.asList(
            // TeeRequestBody forwards these to the app's body; OkHttp calls them only from 3.14 on,
            // the version that added them
            "okhttp3/RequestBody.isOneShot()Z",
            "okhttp3/RequestBody.isDuplex()Z"));

    /**
     * EventListener callbacks newer than 3.9.0: ForwardingEventListener (compiled against 5.5.0)
     * overrides every callback up to 5.5 and forwards each to the app's listener and ours. OkHttp
     * calls only the callbacks its version has, so the forwarding of a newer one never runs on an
     * older OkHttp. Checked by owner and name, whatever the descriptor.
     */
    private static final Set<String> NEWER_CALLBACKS = new HashSet<>(Arrays.asList(
            "requestFailed", "responseFailed", "canceled", "satisfactionFailure", "cacheHit",
            "cacheMiss", "cacheConditionalHit", "proxySelectStart", "proxySelectEnd", "retryDecision",
            "followUpDecision", "dispatcherQueueStart", "dispatcherQueueEnd"));

    @Test
    public void everyOkHttpAndOkioReferenceExistsInOkHttp39() throws Exception {
        assumeTrue("runs in the OkHttp 3.9.0 suite, with the Okio it ships",
                "3.9.0".equals(System.getProperty("trafficpolice.okhttp"))
                        && "shipped".equals(System.getProperty("trafficpolice.okio")));
        Set<File> roots = new LinkedHashSet<>();
        roots.add(classRoot(CaptureInterceptor.class));
        // compiled in its own source set (against OkHttp 5.5.0), so not on this test's compile path
        roots.add(classRoot(Class.forName("io.trafficpolice.capture.okhttp.ForwardingEventListener")));
        List<Named> classes = new ArrayList<>();
        for (File root : roots) {
            collect(root, classes);
        }
        assertTrue("class files found: " + classes.size(), classes.size() > 20);

        Set<String> missing = new TreeSet<>();
        Set<String> guardsUsed = new HashSet<>();
        int checked = 0;
        for (Named f : classes) {
            for (Ref ref : refs(f.bytes)) {
                if (!ref.owner.startsWith("okhttp3/") && !ref.owner.startsWith("okio/")) continue;
                checked++;
                String key = ref.key();
                if (exists(ref)) continue;
                if (GUARDED.contains(key)) {
                    guardsUsed.add(key);
                } else if (ref.owner.equals("okhttp3/EventListener") && NEWER_CALLBACKS.contains(ref.name)) {
                    guardsUsed.add("callback " + ref.name);
                } else {
                    missing.add(key + "  (in " + f.name + ")");
                }
            }
        }
        assertTrue("references checked: " + checked, checked > 100);
        assertTrue("not in OkHttp 3.9.0 / Okio 1.13.0:\n  " + String.join("\n  ", missing), missing.isEmpty());
        Set<String> stale = new TreeSet<>(GUARDED);
        stale.removeAll(guardsUsed);
        assertTrue("guards no longer needed (remove them): " + stale, stale.isEmpty());
    }

    /** Where a class was loaded from: a directory of class files or a jar. */
    private static File classRoot(Class<?> c) throws Exception {
        return new File(c.getProtectionDomain().getCodeSource().getLocation().toURI());
    }

    private static final class Named {
        final String name;
        final byte[] bytes;

        Named(String name, byte[] bytes) {
            this.name = name;
            this.bytes = bytes;
        }
    }

    private static void collect(File root, List<Named> out) throws IOException {
        if (root.isFile()) {
            try (java.util.jar.JarFile jar = new java.util.jar.JarFile(root)) {
                for (java.util.jar.JarEntry e : java.util.Collections.list(jar.entries())) {
                    if (e.getName().endsWith(".class")) {
                        try (java.io.InputStream in = jar.getInputStream(e)) {
                            out.add(new Named(e.getName(), readAll(in)));
                        }
                    }
                }
            }
            return;
        }
        File[] files = root.listFiles();
        if (files == null) return;
        for (File f : files) {
            if (f.isDirectory()) {
                collect(f, out);
            } else if (f.getName().endsWith(".class")) {
                out.add(new Named(f.getName(), Files.readAllBytes(f.toPath())));
            }
        }
    }

    private static byte[] readAll(java.io.InputStream in) throws IOException {
        java.io.ByteArrayOutputStream out = new java.io.ByteArrayOutputStream();
        byte[] buf = new byte[8192];
        for (int n; (n = in.read(buf)) > 0; ) out.write(buf, 0, n);
        return out.toByteArray();
    }

    /** A class, field or method reference from a constant pool (`kind` 7, 9, 10 or 11). */
    private static final class Ref {
        final int kind;
        final String owner;
        final String name;
        final String desc;

        Ref(int kind, String owner, String name, String desc) {
            this.kind = kind;
            this.owner = owner;
            this.name = name;
            this.desc = desc;
        }

        String key() {
            return kind == 7 ? owner : owner + "." + name + desc;
        }
    }

    /** The class, field and method references in a class file's constant pool. */
    private static List<Ref> refs(byte[] bytes) throws IOException {
        DataInputStream in = new DataInputStream(new ByteArrayInputStream(bytes));
        if (in.readInt() != 0xCAFEBABE) throw new IOException("not a class file");
        in.readUnsignedShort();
        in.readUnsignedShort();
        int count = in.readUnsignedShort();
        int[] tags = new int[count];
        Object[] pool = new Object[count];
        for (int i = 1; i < count; i++) {
            int tag = in.readUnsignedByte();
            tags[i] = tag;
            switch (tag) {
                case 1: pool[i] = in.readUTF(); break;
                case 3: case 4: in.readInt(); break;
                case 5: case 6: in.readLong(); i++; break;
                case 7: case 8: case 16: case 19: case 20: pool[i] = in.readUnsignedShort(); break;
                case 9: case 10: case 11: case 12: case 17: case 18:
                    pool[i] = new int[] {in.readUnsignedShort(), in.readUnsignedShort()};
                    break;
                case 15: in.readUnsignedByte(); in.readUnsignedShort(); break;
                default: throw new IOException("constant pool tag " + tag);
            }
        }
        List<Ref> out = new ArrayList<>();
        for (int i = 1; i < count; i++) {
            if (tags[i] == 7) {
                String name = (String) pool[(Integer) pool[i]];
                // array types name their element type
                String element = name.replaceFirst("^\\[+", "");
                if (element.startsWith("L") && element.endsWith(";")) element = element.substring(1, element.length() - 1);
                out.add(new Ref(7, element, "", ""));
            } else if (tags[i] == 9 || tags[i] == 10 || tags[i] == 11) {
                int[] r = (int[]) pool[i];
                String owner = (String) pool[(Integer) pool[r[0]]];
                int[] nat = (int[]) pool[r[1]];
                out.add(new Ref(tags[i], owner, (String) pool[nat[0]], (String) pool[nat[1]]));
            }
        }
        return out;
    }

    private static boolean exists(Ref ref) {
        Class<?> owner;
        try {
            owner = Class.forName(ref.owner.replace('/', '.'), false, ApiBaselineTest.class.getClassLoader());
        } catch (ClassNotFoundException | LinkageError e) {
            return false;
        }
        switch (ref.kind) {
            case 7:
                return true;
            case 9:
                for (Class<?> c : hierarchy(owner)) {
                    for (Field f : c.getDeclaredFields()) {
                        if (f.getName().equals(ref.name) && descriptor(f.getType()).equals(ref.desc)) return true;
                    }
                }
                return false;
            default:
                if (ref.name.equals("<init>")) {
                    for (Constructor<?> k : owner.getDeclaredConstructors()) {
                        if (descriptor(k.getParameterTypes(), void.class).equals(ref.desc)) return true;
                    }
                    return false;
                }
                for (Class<?> c : hierarchy(owner)) {
                    for (Method m : c.getDeclaredMethods()) {
                        if (m.getName().equals(ref.name)
                                && descriptor(m.getParameterTypes(), m.getReturnType()).equals(ref.desc)) {
                            return true;
                        }
                    }
                }
                return false;
        }
    }

    /** The class, its superclasses and every interface they implement. */
    private static List<Class<?>> hierarchy(Class<?> start) {
        List<Class<?>> out = new ArrayList<>();
        Deque<Class<?>> todo = new ArrayDeque<>();
        todo.add(start);
        while (!todo.isEmpty()) {
            Class<?> c = todo.poll();
            if (out.contains(c)) continue;
            out.add(c);
            if (c.getSuperclass() != null) todo.add(c.getSuperclass());
            todo.addAll(Arrays.asList(c.getInterfaces()));
        }
        if (start.isInterface()) out.add(Object.class);
        return out;
    }

    private static String descriptor(Class<?>[] params, Class<?> ret) {
        StringBuilder b = new StringBuilder("(");
        for (Class<?> p : params) b.append(descriptor(p));
        return b.append(')').append(descriptor(ret)).toString();
    }

    private static String descriptor(Class<?> c) {
        if (c.isArray()) return "[" + descriptor(c.getComponentType());
        if (c == void.class) return "V";
        if (c == int.class) return "I";
        if (c == long.class) return "J";
        if (c == boolean.class) return "Z";
        if (c == byte.class) return "B";
        if (c == char.class) return "C";
        if (c == short.class) return "S";
        if (c == float.class) return "F";
        if (c == double.class) return "D";
        return "L" + c.getName().replace('.', '/') + ";";
    }
}
