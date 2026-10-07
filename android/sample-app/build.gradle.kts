// A small app that exercises every capture path: Retrofit suspend calls, raw OkHttp with
// execute() and enqueue(), HttpURLConnection, streaming, uploads, redirects, errors, timeouts,
// cancellation, TLS, a WebSocket, gRPC, and a second process. It talks to an HTTP and an HTTPS server inside the app
// itself (OkHttp's MockWebServer), so it works on any device or emulator without a network.

plugins {
    alias(libs.plugins.android.application)
}

android {
    namespace = "io.trafficpolice.sample"
    compileSdk = 37

    defaultConfig {
        applicationId = "io.trafficpolice.sample"
        minSdk = 26
        targetSdk = 37
        versionCode = 1
        versionName = "0.1.0"
    }

    buildTypes {
        release {
            // release builds use capture-noop; signed with the debug key so they install for testing
            signingConfig = signingConfigs.getByName("debug")
        }
        // A release-like build that wrongly ships the real capture library: capture must refuse
        // to start because the app is not debuggable (ARCHITECTURE.md §7).
        create("nondebuggable") {
            initWith(getByName("release"))
            applicationIdSuffix = ".nondebuggable"
            isDebuggable = false
            matchingFallbacks += listOf("release")
        }
        // The app with no traffic-police code at all, for attach mode (ARCHITECTURE.md §4.7).
        create("plain") {
            initWith(getByName("debug"))
            applicationIdSuffix = ".plain"
        }
    }

    // Capture.kt: the builds with the library call it; the plain build has a stand-in
    sourceSets {
        for (withLibrary in listOf("debug", "release", "nondebuggable")) {
            getByName(withLibrary).kotlin.directories.add("src/withLibrary/java")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    packaging {
        resources.excludes += listOf("META-INF/versions/9/module-info.class", "META-INF/*.kotlin_module")
    }
}

dependencies {
    debugImplementation(project(":capture"))
    releaseImplementation(project(":capture-noop"))
    "nondebuggableImplementation"(project(":capture"))

    implementation(libs.kotlin.stdlib)
    implementation(libs.okhttp.latest)
    implementation(libs.okhttp.tls)
    implementation(libs.mockwebserver3)
    implementation(libs.grpc.okhttp)
    implementation(libs.grpc.stub)
    implementation(libs.retrofit)
    implementation(libs.retrofit.scalars)
    implementation(libs.coroutines.android)
}
