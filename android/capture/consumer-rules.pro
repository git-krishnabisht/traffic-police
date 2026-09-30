# The capture runtime is looked up by name (the provider) and reflects on OkHttp; keep it whole
# in debug builds that are minified.
-keep class io.trafficpolice.** { *; }
-dontwarn okhttp3.**
-dontwarn okio.**
