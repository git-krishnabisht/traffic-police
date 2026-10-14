# The pass-through classes implement OkHttp's and gRPC's types, which an app without OkHttp or
# gRPC does not have. R8 leaves them out unless the app's rules keep them (-dontshrink, or a rule
# that keeps every class); then those types are missing classes, and R8 fails the build.
-dontwarn okhttp3.**
-dontwarn io.grpc.**
