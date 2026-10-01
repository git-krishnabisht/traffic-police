//! Realistic payloads, headers, threads and stacks for the demo device.

use std::io::Write;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use traffic_police_proto::Headers;
use traffic_police_proto::msg::{StackFrame, ThreadInfo};

/// Deterministic PRNG (SplitMix64), so a seed always produces the same session.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9e37_79b9_7f4a_7c15)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `lo..=hi`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next_u64() % (hi - lo + 1)
    }

    pub fn chance(&mut self, percent: u64) -> bool {
        self.next_u64() % 100 < percent
    }

    pub fn hex(&mut self, len: usize) -> String {
        (0..len).map(|_| char::from_digit((self.next_u64() % 16) as u32, 16).unwrap()).collect()
    }

    pub fn b64(&mut self, len: usize) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        (0..len).map(|_| A[(self.next_u64() % 64) as usize] as char).collect()
    }

    pub fn uuid(&mut self) -> String {
        let h = self.hex(32);
        format!("{}-{}-4{}-a{}-{}", &h[0..8], &h[8..12], &h[13..16], &h[17..20], &h[20..32])
    }

    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(len + 8);
        while v.len() < len {
            v.extend_from_slice(&self.next_u64().to_le_bytes());
        }
        v.truncate(len);
        v
    }
}

pub fn gzip(data: &[u8]) -> Bytes {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).expect("in-memory write");
    Bytes::from(e.finish().expect("in-memory gzip"))
}

pub fn jwt(rng: &mut Rng, sub: &str, iat_s: i64) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT","kid":"shop-2026-09"}"#);
    let claims = serde_json::json!({
        "sub": sub,
        "iss": "https://auth.example.com",
        "aud": "shop-api",
        "scope": "catalog orders",
        "iat": iat_s,
        "exp": iat_s + 3600,
    });
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    format!("{header}.{payload}.{}", rng.b64(43))
}

/// A small generated avatar (PNG).
pub fn avatar_png(seed: u64) -> Bytes {
    let size = 96u32;
    let (r0, g0, b0) = ((seed % 200) as u8 + 30, ((seed >> 8) % 200) as u8 + 30, ((seed >> 16) % 200) as u8 + 30);
    let img = image::RgbImage::from_fn(size, size, |x, y| {
        let (dx, dy) = (x as f32 - 48.0, y as f32 - 40.0);
        let head = dx * dx + dy * dy < 22.0 * 22.0;
        let (sx, sy) = (x as f32 - 48.0, y as f32 - 100.0);
        let body = sx * sx / 1.6 + sy * sy < 42.0 * 42.0;
        if head || body {
            image::Rgb([240, 236, 228])
        } else {
            let t = (x + y) as f32 / (2.0 * size as f32);
            image::Rgb([(r0 as f32 * (1.0 - t)) as u8, (g0 as f32 * (0.6 + 0.4 * t)) as u8, b0])
        }
    });
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Png).expect("in-memory png");
    Bytes::from(out.into_inner())
}

// --- protobuf encoding (enough for the demo's metrics batch) ---------------------------------

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn pb_varint(out: &mut Vec<u8>, field: u64, v: u64) {
    varint(out, field << 3);
    varint(out, v);
}

fn pb_bytes(out: &mut Vec<u8>, field: u64, b: &[u8]) {
    varint(out, (field << 3) | 2);
    varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn pb_double(out: &mut Vec<u8>, field: u64, v: f64) {
    varint(out, (field << 3) | 1);
    out.extend_from_slice(&v.to_le_bytes());
}

/// `MetricsBatch { device_id=1, repeated Metric metrics=2 { name=1, value=2 (double), ts_ms=3 }, app=3 }`
pub fn metrics_batch(rng: &mut Rng, ts_ms: u64) -> Bytes {
    let mut out = Vec::new();
    pb_bytes(&mut out, 1, format!("dev-{}", rng.hex(12)).as_bytes());
    for (name, v) in
        [("app.start_ms", 812.0), ("screen.render_ms", 16.4), ("image.decode_ms", 23.0), ("cart.sync_ms", 184.0)]
    {
        let mut m = Vec::new();
        pb_bytes(&mut m, 1, name.as_bytes());
        pb_double(&mut m, 2, v);
        pb_varint(&mut m, 3, ts_ms);
        pb_bytes(&mut out, 2, &m);
    }
    pb_bytes(&mut out, 3, b"shop-android/3.8.0");
    Bytes::from(out)
}

/// `Ack { accepted=1, status=2 }`
pub fn metrics_ack() -> Bytes {
    let mut out = Vec::new();
    pb_varint(&mut out, 1, 4);
    pb_bytes(&mut out, 2, b"ok");
    Bytes::from(out)
}

// --- headers --------------------------------------------------------------------------------

pub fn h(pairs: &[(&str, &str)]) -> Headers {
    pairs.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect()
}

pub fn http_date(wall_ms: i64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let secs = wall_ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    // civil from days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        DAYS[days.rem_euclid(7) as usize],
        d,
        MONTHS[(m - 1) as usize],
        y,
        tod / 3600,
        (tod / 60) % 60,
        tod % 60
    )
}

// --- threads and stacks ---------------------------------------------------------------------

pub fn thread(name: &str, id: i64, origin: &str) -> ThreadInfo {
    ThreadInfo { name: name.into(), id, origin: Some(origin.into()) }
}

fn f(c: &str, m: &str, file: &str, line: i32) -> StackFrame {
    StackFrame { c: c.into(), m: m.into(), f: Some(file.into()), l: (line > 0).then_some(line) }
}

/// Frames OkHttp shows at `callStart` for an `enqueue()` from Retrofit inside a coroutine.
fn retrofit_suspend_frames() -> Vec<StackFrame> {
    vec![
        f("okhttp3.internal.connection.RealCall", "callStart", "RealCall.kt", 171),
        f("okhttp3.internal.connection.RealCall", "enqueue", "RealCall.kt", 163),
        f("retrofit2.OkHttpCall", "enqueue", "OkHttpCall.java", 193),
        f("retrofit2.KotlinExtensions", "await", "KotlinExtensions.kt", 37),
        f("retrofit2.HttpServiceMethod$SuspendForBody", "adapt", "HttpServiceMethod.java", 243),
        f("retrofit2.HttpServiceMethod", "invoke", "HttpServiceMethod.java", 146),
        f("retrofit2.Retrofit$1", "invoke", "Retrofit.java", 160),
        f("java.lang.reflect.Proxy", "invoke", "Proxy.java", 1006),
    ]
}

fn coroutine_tail(worker: i64) -> Vec<StackFrame> {
    let _ = worker;
    vec![
        f("kotlin.coroutines.jvm.internal.BaseContinuationImpl", "resumeWith", "ContinuationImpl.kt", 33),
        f("kotlinx.coroutines.DispatchedTask", "run", "DispatchedTask.kt", 101),
        f("kotlinx.coroutines.internal.LimitedDispatcher$Worker", "run", "LimitedDispatcher.kt", 113),
        f("kotlinx.coroutines.scheduling.TaskImpl", "run", "Tasks.kt", 89),
        f("kotlinx.coroutines.scheduling.CoroutineScheduler", "runSafely", "CoroutineScheduler.kt", 586),
        f("kotlinx.coroutines.scheduling.CoroutineScheduler$Worker", "executeTask", "CoroutineScheduler.kt", 820),
        f("kotlinx.coroutines.scheduling.CoroutineScheduler$Worker", "runWorker", "CoroutineScheduler.kt", 717),
        f("kotlinx.coroutines.scheduling.CoroutineScheduler$Worker", "run", "CoroutineScheduler.kt", 704),
    ]
}

/// A Retrofit suspend call made from `class.method` (the app's call site).
pub fn app_stack(api_method: &str, caller_class: &str, caller_method: &str, file: &str, line: i32) -> Vec<StackFrame> {
    let mut v = retrofit_suspend_frames();
    v.push(f("$Proxy14", api_method, "", 0));
    v.push(f(caller_class, caller_method, file, line));
    v.push(f(&format!("{caller_class}${caller_method}$1"), "invokeSuspend", file, line - 3));
    v.extend(coroutine_tail(0));
    v
}

pub fn telemetry_stack() -> Vec<StackFrame> {
    vec![
        f("okhttp3.internal.connection.RealCall", "callStart", "RealCall.kt", 171),
        f("okhttp3.internal.connection.RealCall", "execute", "RealCall.kt", 151),
        f("com.example.shop.telemetry.EventUploader", "post", "EventUploader.kt", 112),
        f("com.example.shop.telemetry.EventUploader", "flush", "EventUploader.kt", 88),
        f("com.example.shop.telemetry.EventUploader$start$1", "run", "EventUploader.kt", 54),
        f("java.util.concurrent.Executors$RunnableAdapter", "call", "Executors.java", 487),
        f("java.util.concurrent.FutureTask", "runAndReset", "FutureTask.java", 307),
        f(
            "java.util.concurrent.ScheduledThreadPoolExecutor$ScheduledFutureTask",
            "run",
            "ScheduledThreadPoolExecutor.java",
            307,
        ),
        f("java.util.concurrent.ThreadPoolExecutor", "runWorker", "ThreadPoolExecutor.java", 1145),
        f("java.util.concurrent.ThreadPoolExecutor$Worker", "run", "ThreadPoolExecutor.java", 644),
        f("java.lang.Thread", "run", "Thread.java", 1012),
    ]
}

pub fn glide_stack() -> Vec<StackFrame> {
    vec![
        f("okhttp3.internal.connection.RealCall", "callStart", "RealCall.kt", 171),
        f("okhttp3.internal.connection.RealCall", "enqueue", "RealCall.kt", 163),
        f("com.bumptech.glide.integration.okhttp3.OkHttpStreamFetcher", "loadData", "OkHttpStreamFetcher.java", 65),
        f("com.bumptech.glide.load.model.MultiModelLoader$MultiFetcher", "loadData", "MultiModelLoader.java", 100),
        f("com.bumptech.glide.load.engine.SourceGenerator", "startNextLoad", "SourceGenerator.java", 70),
        f("com.bumptech.glide.load.engine.SourceGenerator", "startNext", "SourceGenerator.java", 63),
        f("com.bumptech.glide.load.engine.DecodeJob", "runGenerators", "DecodeJob.java", 311),
        f("com.bumptech.glide.load.engine.DecodeJob", "run", "DecodeJob.java", 234),
        f("java.util.concurrent.ThreadPoolExecutor", "runWorker", "ThreadPoolExecutor.java", 1145),
        f("java.util.concurrent.ThreadPoolExecutor$Worker", "run", "ThreadPoolExecutor.java", 644),
        f("java.lang.Thread", "run", "Thread.java", 1012),
        f(
            "com.bumptech.glide.load.engine.executor.GlideExecutor$DefaultThreadFactory$1",
            "run",
            "GlideExecutor.java",
            424,
        ),
    ]
}

pub fn volley_stack() -> Vec<StackFrame> {
    vec![
        f("java.net.URL", "openConnection", "URL.java", 1006),
        f("com.android.volley.toolbox.HurlStack", "createConnection", "HurlStack.java", 219),
        f("com.android.volley.toolbox.HurlStack", "executeRequest", "HurlStack.java", 102),
        f("com.android.volley.toolbox.BasicNetwork", "performRequest", "BasicNetwork.java", 104),
        f("com.android.volley.NetworkDispatcher", "processRequest", "NetworkDispatcher.java", 132),
        f("com.android.volley.NetworkDispatcher", "run", "NetworkDispatcher.java", 111),
    ]
}

pub fn download_stack() -> Vec<StackFrame> {
    vec![
        f("okhttp3.internal.connection.RealCall", "callStart", "RealCall.kt", 171),
        f("okhttp3.internal.connection.RealCall", "execute", "RealCall.kt", 151),
        f("com.example.shop.offline.CatalogDownloader", "fetch", "CatalogDownloader.kt", 91),
        f("com.example.shop.offline.CatalogDownloader", "download", "CatalogDownloader.kt", 57),
        f("com.example.shop.offline.CatalogDownloader$ensureLatest$1", "run", "CatalogDownloader.kt", 33),
        f("java.util.concurrent.ThreadPoolExecutor", "runWorker", "ThreadPoolExecutor.java", 1145),
        f("java.util.concurrent.ThreadPoolExecutor$Worker", "run", "ThreadPoolExecutor.java", 644),
        f("java.lang.Thread", "run", "Thread.java", 1012),
    ]
}

pub fn login_stack() -> Vec<StackFrame> {
    app_stack("token", "com.example.shop.auth.TokenRepository", "login", "TokenRepository.kt", 34)
}

pub const ERROR_PAGE: &str = "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"><title>500 Internal Server Error</title>\
<style>body{font-family:sans-serif;margin:2em}</style></head><body><h1>Internal Server Error</h1>\
<p>The server encountered an error and could not complete your request.</p><hr><address>envoy</address></body></html>\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_dates_and_rng_are_stable() {
        assert_eq!(http_date(1_790_658_651_000), "Tue, 29 Sep 2026 05:10:51 GMT");
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        assert_eq!(a.hex(16), b.hex(16));
        assert!(a.range(3, 5) >= 3);
        assert!(avatar_png(1).starts_with(b"\x89PNG"));
    }
}
