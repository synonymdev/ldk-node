## Publishing
Publishing new version guide.

1. Run in root dir
   `sh scripts/uniffi_bindgen_generate_kotlin_android.sh`

1. Update `version` in `bindings/kotlin/ldk-node-android/gradle.properties`.

1. Commit

1. Push new branch (or new tag)

1. In the android project:

    - in `settings.gradle.kts` add GitHub Packages repository:

        ```kt
        dependencyResolutionManagement {
            repositories {
                google()
                mavenCentral()
                maven {
                    url = uri("https://maven.pkg.github.com/synonymdev/ldk-node")
                    credentials {
                        username = providers.gradleProperty("gpr.user").orNull ?: System.getenv("GITHUB_ACTOR")
                        password = providers.gradleProperty("gpr.key").orNull ?: System.getenv("GITHUB_TOKEN")
                    }
                }
            }
        ```
    - add dependency in `libs.versions.toml`:
        ```toml
        ldk-node-android = { module = "com.synonym:ldk-node-android", version = "0.7.0-rc.26" }
        ```
    - Run `Sync project with gradle files` action in android studio
