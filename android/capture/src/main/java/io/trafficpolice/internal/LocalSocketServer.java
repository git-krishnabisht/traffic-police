package io.trafficpolice.internal;

import android.net.Credentials;
import android.net.LocalServerSocket;
import android.net.LocalSocket;
import android.util.Log;
import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.Transport;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.util.LinkedHashMap;
import java.util.Map;

/**
 * Listens on the abstract socket {@code @traffic-police_<package>_<pid>} (PROTOCOL.md §2). Only
 * peers running as root (UID 0) or the shell user (UID 2000, which is what adbd connects as for
 * {@code adb forward}) are served; anyone else, such as another app, is closed before a single
 * byte is sent.
 */
final class LocalSocketServer implements Runnable {
    private static final int ROOT_UID = 0;
    private static final int SHELL_UID = 2000;

    private final CaptureRuntime rt;
    private final String name;
    private boolean warnedRejected;

    private LocalSocketServer(CaptureRuntime rt, String name) {
        this.rt = rt;
        this.name = name;
    }

    static void start(CaptureRuntime rt, String name) {
        Thread t = new Thread(new LocalSocketServer(rt, name), "traffic-police-server");
        t.setDaemon(true);
        t.start();
    }

    @Override
    public void run() {
        LocalServerSocket server;
        try {
            server = new LocalServerSocket(name);
        } catch (IOException e) {
            Map<String, String> data = new LinkedHashMap<>();
            data.put("socket", "@" + name);
            rt.diag("error", "internal_error", "cannot listen on @" + name + ": " + e.getMessage(), data);
            Log.e(AndroidRuntime.TAG, "cannot listen on @" + name, e);
            return;
        }
        while (true) {
            LocalSocket socket;
            try {
                socket = server.accept();
            } catch (IOException e) {
                Log.w(AndroidRuntime.TAG, "accept failed", e);
                try {
                    Thread.sleep(250);
                } catch (InterruptedException ie) {
                    return;
                }
                continue;
            }
            int uid;
            try {
                Credentials peer = socket.getPeerCredentials();
                uid = peer.getUid();
            } catch (IOException e) {
                close(socket);
                continue;
            }
            if (uid != ROOT_UID && uid != SHELL_UID) {
                close(socket);
                if (!warnedRejected) {
                    warnedRejected = true;
                    Log.w(AndroidRuntime.TAG, "refused a connection from uid " + uid + " (only adb and root may connect)");
                }
                continue;
            }
            rt.serve(new SocketTransport(socket));
        }
    }

    private static void close(LocalSocket s) {
        try {
            s.close();
        } catch (IOException ignored) {
            // nothing to do
        }
    }

    private static final class SocketTransport implements Transport {
        private final LocalSocket socket;

        SocketTransport(LocalSocket socket) {
            this.socket = socket;
        }

        @Override
        public InputStream input() throws IOException {
            return socket.getInputStream();
        }

        @Override
        public OutputStream output() throws IOException {
            return socket.getOutputStream();
        }

        @Override
        public void setReadTimeout(int millis) throws IOException {
            socket.setSoTimeout(millis);
        }

        @Override
        public void close() throws IOException {
            // closing alone does not wake a thread blocked in read on a LocalSocket
            try {
                socket.shutdownInput();
            } catch (IOException ignored) {
                // already shut
            }
            try {
                socket.shutdownOutput();
            } catch (IOException ignored) {
                // already shut
            }
            socket.close();
        }
    }
}
