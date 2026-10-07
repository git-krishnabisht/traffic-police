package com.android.tools.appinspection.network.okhttp;

import java.io.IOException;
import okhttp3.Interceptor;
import okhttp3.Response;

/** A stand-in for Android Studio's network interceptor: its class name is what gives it away. */
public final class OkHttp3Interceptor implements Interceptor {
    @Override
    public Response intercept(Chain chain) throws IOException {
        return chain.proceed(chain.request());
    }
}
