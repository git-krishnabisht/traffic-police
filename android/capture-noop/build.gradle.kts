// io.trafficpolice:capture-noop: the same public API as :capture, doing nothing. Use it as
// releaseImplementation so a release build contains no capture code at all.

plugins {
    alias(libs.plugins.android.library)
    `maven-publish`
}

android {
    namespace = "io.trafficpolice.noop"
    compileSdk = 37

    defaultConfig {
        minSdk = 21
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }

    publishing {
        singleVariant("release") {
            withSourcesJar()
        }
    }
}

// Java 8 is the target on purpose (apps with old toolchains can consume it)
tasks.withType<JavaCompile>().configureEach {
    options.compilerArgs.add("-Xlint:-options")
}

dependencies {
    compileOnly(libs.okhttp.baseline) { exclude(group = "com.squareup.okio") }
    compileOnly(libs.okio.baseline)
    compileOnly(libs.grpc.api.baseline)
}

publishing {
    publications {
        create<MavenPublication>("release") {
            artifactId = "capture-noop"
            afterEvaluate { from(components["release"]) }
        }
    }
}
