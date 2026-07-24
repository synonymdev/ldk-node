import java.io.ByteArrayOutputStream
import java.io.File
import java.util.zip.ZipFile

plugins {
    id("com.android.library")
    kotlin("android")
    kotlin("plugin.serialization")

    id("maven-publish")
    id("signing")
    id("org.jlleitschuh.gradle.ktlint") version "11.6.1"
}

repositories {
    mavenCentral()
    google()
}

android {
    namespace = "org.lightningdevkit.ldknode"
    compileSdk = 34

    defaultConfig {
        minSdk = 24
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        consumerProguardFiles("consumer-rules.pro")
    }

    buildTypes {
        getByName("release") {
            isMinifyEnabled = false
            proguardFiles(file("proguard-android-optimize.txt"), file("proguard-rules.pro"))
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }

    kotlinOptions {
        jvmTarget = "1.8"
    }

    publishing {
        singleVariant("release") {
            withSourcesJar()
            withJavadocJar()
        }
    }
}

dependencies {
    implementation("net.java.dev.jna:jna:5.12.0@aar")
    implementation("org.jetbrains.kotlin:kotlin-stdlib-jdk7")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.10.2")
    implementation("androidx.appcompat:appcompat:1.4.0")
    implementation("androidx.core:core-ktx:1.7.0")
    implementation("org.jetbrains.kotlinx:atomicfu:0.27.0")
    implementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.9.0")
    api("org.slf4j:slf4j-api:1.7.30")
}

val androidNativeAbis = listOf("armeabi-v7a", "arm64-v8a", "x86_64")

fun executableFromPath(name: String): String? {
    return System.getenv("PATH")
        ?.split(File.pathSeparator)
        ?.asSequence()
        ?.map { File(it, name) }
        ?.firstOrNull { it.canExecute() }
        ?.absolutePath
}

fun findReadelf(): String {
    executableFromPath("llvm-readelf")?.let { return it }
    executableFromPath("readelf")?.let { return it }

    return listOf("ANDROID_NDK_ROOT", "ANDROID_NDK_HOME", "NDK_HOME")
        .mapNotNull { System.getenv(it) }
        .map { File(it, "toolchains/llvm/prebuilt") }
        .firstNotNullOfOrNull { prebuiltDir ->
            if (!prebuiltDir.isDirectory) return@firstNotNullOfOrNull null

            prebuiltDir
                .walkTopDown()
                .firstOrNull { it.name == "llvm-readelf" && it.canExecute() }
                ?.absolutePath
        }
        ?: throw GradleException(
            "llvm-readelf or readelf is required to validate Android native debug symbols"
        )
}

fun Project.runReadelf(readelf: String, vararg args: String): Pair<Int, String> {
    val stdout = ByteArrayOutputStream()
    val stderr = ByteArrayOutputStream()
    val result = exec {
        commandLine(readelf, *args)
        standardOutput = stdout
        errorOutput = stderr
        isIgnoreExitValue = true
    }

    return result.exitValue to stdout.toString().ifBlank { stderr.toString() }
}

fun String.parseElfAlignment(): Long {
    return if (startsWith("0x")) {
        removePrefix("0x").toLong(16)
    } else {
        toLong()
    }
}

fun Project.validateReleaseNativeLibrary(
    readelf: String,
    abi: String,
    library: File,
    artifact: String
) {
    val (sectionsExit, sections) = runReadelf(readelf, "-S", library.absolutePath)
    if (sectionsExit != 0) {
        throw GradleException(
            "Unable to inspect Android native library sections: " +
                "ABI=$abi library='${library.path}' artifact=$artifact"
        )
    }
    if (Regex("""\.debug_""").containsMatchIn(sections)) {
        throw GradleException(
            "Android release native library still contains .debug_* sections: " +
                "ABI=$abi library='${library.path}' artifact=$artifact"
        )
    }

    val wideHeaders = runReadelf(readelf, "-W", "-l", library.absolutePath)
    val headers = if (wideHeaders.first == 0) {
        wideHeaders.second
    } else {
        val fallbackHeaders = runReadelf(readelf, "-l", library.absolutePath)
        if (fallbackHeaders.first != 0) {
            throw GradleException(
                "Unable to inspect Android native library headers: " +
                    "ABI=$abi library='${library.path}' artifact=$artifact"
            )
        }
        fallbackHeaders.second
    }

    val programHeaders = headers
        .lineSequence()
        .map { it.trim().split(Regex("""\s+""")) }
        .filter { it.isNotEmpty() }
        .toList()
    val loadAlignments = programHeaders
        .filter { it.first() == "LOAD" }
        .map { it.last().parseElfAlignment() }
    val relroEnds = programHeaders
        .filter { it.first() == "GNU_RELRO" && it.size >= 6 }
        .map { it[2].parseElfAlignment() + it[5].parseElfAlignment() }

    val compatible = loadAlignments.isNotEmpty() &&
        loadAlignments.all { it >= 16_384 } &&
        relroEnds.isNotEmpty() &&
        relroEnds.all { it % 16_384 == 0L }
    val detectedLoad = loadAlignments.joinToString(prefix = "[", postfix = "]") { "0x${it.toString(16)}" }
    val detectedRelro = relroEnds.joinToString(prefix = "[", postfix = "]") { "0x${it.toString(16)}" }

    if (!compatible) {
        throw GradleException(
            "Android native library is not 16 KB page-size compatible: " +
                "ABI=$abi library='${library.path}' artifact=$artifact " +
                "LOAD=$detectedLoad GNU_RELRO_end=$detectedRelro"
        )
    }

    logger.lifecycle(
        "Validated Android native library: ABI=$abi library='${library.path}' " +
            "artifact=$artifact LOAD=$detectedLoad GNU_RELRO_end=$detectedRelro"
    )
}

val validateReleaseNativeLibraries by tasks.registering {
    group = "verification"
    description = "Validates source release JNI libraries are stripped and 16 KB page-size compatible."

    doLast {
        val readelf = findReadelf()

        androidNativeAbis.forEach { abi ->
            val lib = layout.projectDirectory.file("src/main/jniLibs/$abi/libldk_node.so").asFile
            if (!lib.isFile) {
                throw GradleException("Android native library missing at '${lib.path}'")
            }

            validateReleaseNativeLibrary(readelf, abi, lib, "source JNI")
        }
    }
}

tasks.matching { it.name == "bundleReleaseAar" }.configureEach {
    dependsOn(validateReleaseNativeLibraries)
}

val validateReleaseAarNativeLibraries by tasks.registering {
    group = "verification"
    description = "Validates every native library in the final release AAR for 16 KB page-size compatibility."
    dependsOn("bundleReleaseAar")

    doLast {
        val readelf = findReadelf()
        val aar = layout.buildDirectory.file("outputs/aar/lib-release.aar").get().asFile
        if (!aar.isFile) {
            throw GradleException("Android release AAR missing at '${aar.path}'")
        }

        temporaryDir.deleteRecursively()
        temporaryDir.mkdirs()
        ZipFile(aar).use { archive ->
            val nativeEntries = archive.entries().asSequence()
                .filter { !it.isDirectory && it.name.startsWith("jni/") && it.name.endsWith(".so") }
                .toList()
            if (nativeEntries.isEmpty()) {
                throw GradleException("Android release AAR contains no native libraries: artifact='${aar.path}'")
            }

            val packagedEntries = nativeEntries.map { it.name }.toSet()
            val missingRequired = androidNativeAbis
                .map { "jni/$it/libldk_node.so" }
                .filterNot(packagedEntries::contains)
            if (missingRequired.isNotEmpty()) {
                throw GradleException(
                    "Android release AAR required native libraries missing: " +
                        "libraries=${missingRequired.joinToString()} artifact='${aar.path}'"
                )
            }

            nativeEntries.forEach { entry ->
                val relativePath = entry.name.removePrefix("jni/")
                val abi = relativePath.substringBefore("/")
                val extracted = temporaryDir.resolve(relativePath)
                extracted.parentFile.mkdirs()
                archive.getInputStream(entry).use { input ->
                    extracted.outputStream().use { output -> input.copyTo(output) }
                }
                validateReleaseNativeLibrary(readelf, abi, extracted, aar.path)
            }
        }
    }
}

tasks.matching { it.name.startsWith("publish") }.configureEach {
    dependsOn(validateReleaseAarNativeLibraries)
}

afterEvaluate {
    publishing {
        publications {
            create<MavenPublication>("maven") {
                val mavenArtifactId = "ldk-node-android"
                groupId = providers.gradleProperty("group").orNull ?: "com.synonym"
                artifactId = mavenArtifactId
                version = providers.gradleProperty("version").orNull ?: "0.0.0"

                from(components["release"])
                artifact(rootProject.layout.projectDirectory.file("native-debug-symbols.zip")) {
                    classifier = "native-debug-symbols"
                    extension = "zip"
                }
                pom {
                    name.set(mavenArtifactId)
                    description.set("LDK Node Android bindings (Synonym fork).")
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
//    val signingKeyId: String? by project
//    val signingKey: String? by project
//    val signingPassword: String? by project
//    useInMemoryPgpKeys(signingKeyId, signingKey, signingPassword)
//    sign(publishing.publications)
}

ktlint {
    filter {
        exclude { entry ->
            entry.file.toString().contains("main")
        }
    }
}
