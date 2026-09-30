#!/usr/bin/env python3
# Generates TrackedHttpURLConnection and TrackedHttpsURLConnection from one method list, so the
# plain and HTTPS wrappers always delegate the same methods the same way.
#
#   python3 android/capture-core/scripts/gen_huc_wrappers.py \
#       android/capture-core/src/main/java/io/trafficpolice/capture/huc
import sys
out_dir = sys.argv[1]

# (return type, name, params [(type, name)], throws, kind)
# kind: plain | getter | getter_throws | special:<expr>
M = [
 ("void","connect",[],"IOException","special:HucCalls.connect(delegate, exchange);"),
 ("void","setConnectTimeout",[("int","timeout")],None,"plain"),
 ("int","getConnectTimeout",[],None,"plain"),
 ("void","setReadTimeout",[("int","timeout")],None,"plain"),
 ("int","getReadTimeout",[],None,"plain"),
 ("URL","getURL",[],None,"plain"),
 ("int","getContentLength",[],None,"getter"),
 ("long","getContentLengthLong",[],None,"getter"),
 ("String","getContentType",[],None,"getter"),
 ("String","getContentEncoding",[],None,"getter"),
 ("long","getExpiration",[],None,"getter"),
 ("long","getDate",[],None,"getter"),
 ("long","getLastModified",[],None,"getter"),
 ("String","getHeaderField",[("String","name")],None,"getter"),
 ("Map<String, List<String>>","getHeaderFields",[],None,"getter"),
 ("int","getHeaderFieldInt",[("String","name"),("int","defaultValue")],None,"getter"),
 ("long","getHeaderFieldLong",[("String","name"),("long","defaultValue")],None,"getter"),
 ("long","getHeaderFieldDate",[("String","name"),("long","defaultValue")],None,"getter"),
 ("String","getHeaderFieldKey",[("int","n")],None,"getter"),
 ("String","getHeaderField",[("int","n")],None,"getter"),
 ("Object","getContent",[],"IOException","getter_throws"),
 ("Object","getContent",[("Class[]","classes")],"IOException","getter_throws_raw"),
 ("Permission","getPermission",[],"IOException","plain"),
 ("InputStream","getInputStream",[],"IOException","special:return HucCalls.getInputStream(delegate, exchange);"),
 ("OutputStream","getOutputStream",[],"IOException","special:return HucCalls.getOutputStream(delegate, exchange);"),
 ("String","toString",[],None,"plain"),
 ("void","setDoInput",[("boolean","doinput")],None,"plain"),
 ("boolean","getDoInput",[],None,"plain"),
 ("void","setDoOutput",[("boolean","dooutput")],None,"plain"),
 ("boolean","getDoOutput",[],None,"plain"),
 ("void","setAllowUserInteraction",[("boolean","allowuserinteraction")],None,"plain"),
 ("boolean","getAllowUserInteraction",[],None,"plain"),
 ("void","setUseCaches",[("boolean","usecaches")],None,"plain"),
 ("boolean","getUseCaches",[],None,"plain"),
 ("void","setIfModifiedSince",[("long","ifmodifiedsince")],None,"plain"),
 ("long","getIfModifiedSince",[],None,"plain"),
 ("boolean","getDefaultUseCaches",[],None,"plain"),
 ("void","setDefaultUseCaches",[("boolean","defaultusecaches")],None,"plain"),
 ("void","setRequestProperty",[("String","key"),("String","value")],None,"plain"),
 ("void","addRequestProperty",[("String","key"),("String","value")],None,"plain"),
 ("String","getRequestProperty",[("String","key")],None,"plain"),
 ("Map<String, List<String>>","getRequestProperties",[],None,"plain"),
 # HttpURLConnection
 ("void","setFixedLengthStreamingMode",[("int","contentLength")],None,"plain"),
 ("void","setFixedLengthStreamingMode",[("long","contentLength")],None,"plain"),
 ("void","setChunkedStreamingMode",[("int","chunklen")],None,"plain"),
 ("void","setInstanceFollowRedirects",[("boolean","followRedirects")],None,"plain"),
 ("boolean","getInstanceFollowRedirects",[],None,"plain"),
 ("void","setRequestMethod",[("String","method")],"ProtocolException","plain"),
 ("String","getRequestMethod",[],None,"plain"),
 ("int","getResponseCode",[],"IOException","special:return HucCalls.responseCode(delegate, exchange);"),
 ("String","getResponseMessage",[],"IOException","special:return HucCalls.responseMessage(delegate, exchange);"),
 ("void","disconnect",[],None,"special:delegate.disconnect();\n        exchange.disconnected();"),
 ("boolean","usingProxy",[],None,"plain"),
 ("InputStream","getErrorStream",[],None,"special:return HucCalls.getErrorStream(delegate, exchange);"),
]
HTTPS = [
 ("String","getCipherSuite",[],None,"plain"),
 ("Certificate[]","getLocalCertificates",[],None,"plain"),
 ("Certificate[]","getServerCertificates",[],"SSLPeerUnverifiedException","plain"),
 ("Principal","getPeerPrincipal",[],"SSLPeerUnverifiedException","plain"),
 ("Principal","getLocalPrincipal",[],None,"plain"),
 ("void","setHostnameVerifier",[("HostnameVerifier","v")],None,"plain"),
 ("HostnameVerifier","getHostnameVerifier",[],None,"plain"),
 ("void","setSSLSocketFactory",[("SSLSocketFactory","sf")],None,"plain"),
 ("SSLSocketFactory","getSSLSocketFactory",[],None,"plain"),
]

def method(ret, name, params, throws, kind, dtype):
    ps = ", ".join(f"{t} {n}" for t, n in params)
    args = ", ".join(n for _, n in params)
    th = f" throws {throws}" if throws else ""
    ann = "    @Override\n"
    if kind == "getter_throws_raw":
        ann += '    @SuppressWarnings("rawtypes") // URLConnection declares a raw Class[]\n'
    head = f"{ann}    public {ret} {name}({ps}){th} {{\n"
    call = f"delegate.{name}({args})"
    if kind == "plain":
        body = f"        {'return ' if ret != 'void' else ''}{call};\n"
    elif kind == "getter":
        body = f"        HucCalls.beforeGetter(exchange);\n        {ret} value = {call};\n        HucCalls.afterGetter(exchange);\n        return value;\n"
    elif kind.startswith("getter_throws"):
        body = (f"        HucCalls.responseCode(delegate, exchange);\n        return {call};\n")
    elif kind.startswith("special:"):
        body = "        " + kind[len("special:"):] + "\n"
    return head + body + "    }\n"

def gen(cls, base, dtype, extra, imports, doc):
    methods = [method(*m, dtype) for m in M + extra]
    return f"""package io.trafficpolice.capture.huc;

{imports}

/**
 * {doc}
 *
 * Generated by {{@code android/capture-core/scripts/gen_huc_wrappers.py}}; edit the script, not
 * this file. Every method either delegates or goes through {{@link HucCalls}}.
 */
public final class {cls} extends {base} {{
    private final {dtype} delegate;
    private final HucExchange exchange;

    public {cls}({dtype} delegate) {{
        super(delegate.getURL());
        this.delegate = delegate;
        this.exchange = new HucExchange(delegate);
    }}

    /** The wrapped connection. */
    public {dtype} delegate() {{
        return delegate;
    }}

""" + "\n".join(methods) + "}\n"

common = """import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.HttpURLConnection;
import java.net.ProtocolException;
import java.net.URL;
import java.security.Permission;
import java.util.List;
import java.util.Map;"""
https_imports = common.replace("import java.net.HttpURLConnection;\n", "") + """
import java.security.Principal;
import java.security.cert.Certificate;
import javax.net.ssl.HostnameVerifier;
import javax.net.ssl.HttpsURLConnection;
import javax.net.ssl.SSLPeerUnverifiedException;
import javax.net.ssl.SSLSocketFactory;"""
open(f"{out_dir}/TrackedHttpURLConnection.java","w").write(gen("TrackedHttpURLConnection","HttpURLConnection","HttpURLConnection",[],common,
 "An {@code HttpURLConnection} that records its exchange (ARCHITECTURE.md §4.3)."))
open(f"{out_dir}/TrackedHttpsURLConnection.java","w").write(gen("TrackedHttpsURLConnection","HttpsURLConnection","HttpsURLConnection",HTTPS,https_imports,
 "An {@code HttpsURLConnection} that records its exchange (ARCHITECTURE.md §4.3)."))
