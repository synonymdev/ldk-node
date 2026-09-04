## Publishing

Follow the [binding release guide](../../README.md). Run `./bindgen.sh` from the repository root; do not run the child generation scripts directly. Commit the generated Kotlin sources, but do not commit `lib/src/main/jniLibs/`. The Android publishing workflow rebuilds the JNI libraries before packaging the AAR.

## Consuming

In the Android project:

- In `settings.gradle.kts`, add the GitHub Packages repository:

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
    }
    ```

- Add the dependency in `libs.versions.toml`:

    ```toml
    ldk-node-android = { module = "com.synonym:ldk-node-android", version = "0.7.0-rc.26" }
    ```

- Run the `Sync project with gradle files` action in Android Studio.
