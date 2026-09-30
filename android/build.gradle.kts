plugins {
    alias(libs.plugins.android.library) apply false
    alias(libs.plugins.android.application) apply false
}

allprojects {
    group = "io.trafficpolice"
    version = "0.1.0"
}

// `./gradlew publishAllPublicationsToBuildRepository` writes the artifacts to build/repo, to test
// their published form without touching ~/.m2 (users run publishToMavenLocal; see the README).
subprojects {
    plugins.withId("maven-publish") {
        extensions.configure<PublishingExtension> {
            repositories {
                maven {
                    name = "build"
                    url = uri(rootProject.layout.buildDirectory.dir("repo"))
                }
            }
        }
    }
}
