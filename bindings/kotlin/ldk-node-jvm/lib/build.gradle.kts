import org.gradle.api.tasks.testing.logging.TestExceptionFormat.FULL
import org.gradle.api.tasks.testing.logging.TestLogEvent.FAILED
import org.gradle.api.tasks.testing.logging.TestLogEvent.PASSED
import org.gradle.api.tasks.testing.logging.TestLogEvent.SKIPPED
import org.gradle.api.tasks.testing.logging.TestLogEvent.STANDARD_ERROR
import org.gradle.api.tasks.testing.logging.TestLogEvent.STANDARD_OUT

plugins {
    // Apply the org.jetbrains.kotlin.jvm Plugin to add support for Kotlin.
    id("org.jetbrains.kotlin.jvm") version "2.2.0"
    id("org.jetbrains.kotlin.plugin.serialization") version "2.2.0"

    // Apply the java-library plugin for API and implementation separation.
    id("java-library")
    id("maven-publish")
    id("signing")
    id("org.jlleitschuh.gradle.ktlint") version "11.6.1"
}

group = "org.lightningdevkit"
// version is read from gradle.properties automatically

repositories {
    // Use Maven Central for resolving dependencies.
    mavenCentral()
}

java {
    withSourcesJar()
    withJavadocJar()
}

dependencies {
    // Use the Kotlin JUnit 5 integration.
    testImplementation("org.jetbrains.kotlin:kotlin-test-junit5")

    // Use the JUnit 5 integration.
    testImplementation("org.junit.jupiter:junit-jupiter-engine:5.9.1")

    // // This dependency is exported to consumers, that is to say found on their compile classpath.
    // api("org.apache.commons:commons-math3:3.6.1")

    // // This dependency is used internally, and not exposed to consumers on their own compile classpath.
    // implementation("com.google.guava:guava:31.1-jre")
    // Align versions of all Kotlin components
    implementation(platform("org.jetbrains.kotlin:kotlin-bom"))

    // Use the Kotlin JDK 8 standard library.
    implementation("org.jetbrains.kotlin:kotlin-stdlib-jdk8")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.10.2")
    implementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.9.0")
    implementation("org.jetbrains.kotlinx:atomicfu:0.27.0")

    implementation("net.java.dev.jna:jna:5.12.0")
}

tasks.named<Test>("test") {
    // Use JUnit Platform for unit tests.
    useJUnitPlatform()

    testLogging {
        events(PASSED, SKIPPED, FAILED, STANDARD_OUT, STANDARD_ERROR)
        exceptionFormat = FULL
        showExceptions = true
        showCauses = true
        showStackTraces = true
        showStandardStreams = true
    }
}

tasks.test {
    doFirst {
        if (project.hasProperty("env") && project.property("env") == "ci") {
            environment("BITCOIN_CLI_BIN", "docker exec ldk-node-bitcoin-1 bitcoin-cli")
            environment("BITCOIND_RPC_USER", "user")
            environment("BITCOIND_RPC_PASSWORD", "pass")
            environment("ESPLORA_ENDPOINT", "http://127.0.0.1:3002")
        } else {
            // Adapt these to your local environment
            environment("BITCOIN_CLI_BIN", "bitcoin-cli")
            environment("BITCOIND_RPC_USER", "")
            environment("BITCOIND_RPC_PASSWORD", "")
            environment("ESPLORA_ENDPOINT", "http://127.0.0.1:3002")
        }
    }
}

afterEvaluate {
    publishing {
        publications {
            create<MavenPublication>("maven") {
                groupId = "org.lightningdevkit"
                artifactId = "ldk-node-jvm"
                version = providers.gradleProperty("version").orNull ?: "0.0.0"

                from(components["java"])
                pom {
                    name.set("ldk-node-jvm")
                    description.set("LDK Node JVM bindings (Synonym fork).")
                    url.set("https://github.com/synonymdev/ldk-node")
                    licenses {
                        license {
                            name.set("MIT")
                            url.set("https://github.com/synonymdev/ldk-node/blob/main/LICENSE-MIT")
                        }
                    }
                    developers {
                        developer {
                            id.set("synonymdev")
                            name.set("Synonym")
                            email.set("noreply@synonym.to")
                        }
                    }
                }
            }
        }
        repositories {
            maven {
                val repo = System.getenv("GITHUB_REPO")
                    ?: providers.gradleProperty("gpr.repo").orNull
                    ?: "synonymdev/ldk-node"
                name = "GitHubPackages"
                url = uri("https://maven.pkg.github.com/$repo")
                credentials {
                    username = System.getenv("GITHUB_ACTOR") ?: providers.gradleProperty("gpr.user").orNull
                    password = System.getenv("GITHUB_TOKEN") ?: providers.gradleProperty("gpr.key").orNull
                }
            }
        }
    }
}

signing {
    // Only sign if signing key is available (not on JitPack)
    setRequired { gradle.taskGraph.hasTask("publish") && project.hasProperty("signingKey") }
    sign(publishing.publications)
}

ktlint {
    filter {
        exclude { entry ->
            entry.file.toString().contains("main")
        }
    }
}
