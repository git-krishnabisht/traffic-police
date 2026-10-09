plugins {
    alias(libs.plugins.android.library) apply false
    alias(libs.plugins.android.application) apply false
}

allprojects {
    group = "io.trafficpolice"
    version = "0.4.0"
}

// `./gradlew publishAllPublicationsToBuildRepository` writes the artifacts to build/repo, to test
// their published form without touching ~/.m2. A release publishes them to the Maven repository
// on GitHub Pages: `publishAllPublicationsToPagesRepository -Ptrafficpolice.pages=<the gh-pages
// branch's maven/ folder>` (release.yml; Gradle keeps maven-metadata.xml listing every version).
val pagesRepo = providers.gradleProperty("trafficpolice.pages").orNull

subprojects {
    plugins.withId("maven-publish") {
        extensions.configure<PublishingExtension> {
            repositories {
                maven {
                    name = "build"
                    url = uri(rootProject.layout.buildDirectory.dir("repo"))
                }
                if (pagesRepo != null) {
                    maven {
                        name = "pages"
                        url = uri(pagesRepo)
                    }
                }
            }
            publications.withType<MavenPublication>().configureEach {
                pom {
                    url.set("https://github.com/git-krishnabisht/traffic-police")
                    scm {
                        url.set("https://github.com/git-krishnabisht/traffic-police")
                        connection.set("scm:git:https://github.com/git-krishnabisht/traffic-police.git")
                    }
                    licenses {
                        license {
                            name.set("The Apache License, Version 2.0")
                            url.set("https://www.apache.org/licenses/LICENSE-2.0")
                        }
                    }
                }
            }
        }
    }
}
