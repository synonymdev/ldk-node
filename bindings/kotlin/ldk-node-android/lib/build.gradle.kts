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
val androidElfIdentities = mapOf(
    "armeabi-v7a" to (1 to 40),
    "arm64-v8a" to (2 to 183),
    "x86" to (1 to 3),
    "x86_64" to (2 to 62)
)

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

fun File.readElfIdentity(): Pair<Int, Int> {
    val header = inputStream().use { input ->
        ByteArray(20).also { bytes ->
            if (input.readNBytes(bytes, 0, bytes.size) != bytes.size) {
                throw GradleException("Android native library has a truncated ELF header: library='$path'")
            }
        }
    }
    if (
        header[0].toInt() != 0x7f ||
        header[1].toInt() != 'E'.code ||
        header[2].toInt() != 'L'.code ||
        header[3].toInt() != 'F'.code ||
        header[5].toInt() != 1
    ) {
        throw GradleException("Android native library has an invalid little-endian ELF header: library='$path'")
    }

    val elfClass = header[4].toInt() and 0xff
    val machine = (header[18].toInt() and 0xff) or ((header[19].toInt() and 0xff) shl 8)
    return elfClass to machine
}

fun requireMatchingAndroidElfIdentity(
    abi: String,
    detectedIdentity: Pair<Int, Int>,
    library: File,
    artifact: String
) {
    val expectedIdentity = androidElfIdentities[abi]
        ?: throw GradleException(
            "Android release native library uses an unsupported ABI directory: " +
                "ABI=$abi library='${library.path}' artifact=$artifact"
        )
    if (detectedIdentity != expectedIdentity) {
        throw GradleException(
            "Android native library ELF identity does not match its ABI directory: " +
                "ABI=$abi library='${library.path}' artifact=$artifact " +
                "ELF_class=${detectedIdentity.first} ELF_machine=${detectedIdentity.second}"
        )
    }
}

fun String.findUnstrippedElfSection(): String? =
    Regex("""\.(?:debug_|zdebug_|symtab)""").find(this)?.value

fun String.parseElfProgramHeaders(): Pair<List<Long>, List<Long>> {
    val programHeaders = lineSequence()
        .map { it.trim().split(Regex("""\s+""")) }
        .filter { it.isNotEmpty() }
        .toList()
    val loadAlignments = programHeaders
        .filter { it.first() == "LOAD" }
        .map { it.last().parseElfAlignment() }
    val relroEnds = programHeaders
        .filter { it.first() == "GNU_RELRO" && it.size >= 6 }
        .map { it[2].parseElfAlignment() + it[5].parseElfAlignment() }
    return loadAlignments to relroEnds
}

fun is16KbElfLayoutCompatible(loadAlignments: List<Long>, relroEnds: List<Long>): Boolean =
    loadAlignments.isNotEmpty() &&
        loadAlignments.all { it >= 16_384 } &&
        relroEnds.isNotEmpty() &&
        relroEnds.all { it % 16_384 == 0L }

fun parseAndroidNativeEntryPath(entryName: String): Pair<String, String> {
    val match = Regex("""^jni/([^/\\]+)/([^/\\]+\.so)$""").matchEntire(entryName)
        ?: throw GradleException("Android release AAR contains an invalid native library path: library=$entryName")
    return match.groupValues[1] to match.groupValues[2]
}

fun requireAndroidNativeEntries(packagedEntries: Set<String>) {
    val missingRequired = androidNativeAbis
        .map { "jni/$it/libldk_node.so" }
        .filterNot(packagedEntries::contains)
    if (missingRequired.isNotEmpty()) {
        throw GradleException(
            "Android release AAR required native libraries missing: libraries=${missingRequired.joinToString()}"
        )
    }
}

fun requireUniqueAndroidNativeEntries(packagedEntries: List<String>) {
    val duplicateEntries = packagedEntries
        .groupingBy { it }
        .eachCount()
        .filterValues { it > 1 }
        .keys
    if (duplicateEntries.isNotEmpty()) {
        throw GradleException(
            "Android release AAR contains duplicate native library entries: " +
                "libraries=${duplicateEntries.joinToString()}"
        )
    }
}

fun Project.validateReleaseNativeLibrary(
    readelf: String,
    abi: String,
    library: File,
    artifact: String
) {
    val detectedIdentity = library.readElfIdentity()
    requireMatchingAndroidElfIdentity(abi, detectedIdentity, library, artifact)

    val (sectionsExit, sections) = runReadelf(readelf, "-S", library.absolutePath)
    if (sectionsExit != 0) {
        throw GradleException(
            "Unable to inspect Android native library sections: " +
                "ABI=$abi library='${library.path}' artifact=$artifact"
        )
    }
    val unstrippedSection = sections.findUnstrippedElfSection()
    if (unstrippedSection != null) {
        throw GradleException(
            "Android release native library contains an unstripped section: " +
                "ABI=$abi library='${library.path}' artifact=$artifact section=$unstrippedSection"
        )
    }

    val wideHeaders = runReadelf(readelf, "-W", "-l", library.absolutePath)
    if (wideHeaders.first != 0) {
        throw GradleException(
            "Unable to inspect Android native library headers: " +
                "ABI=$abi library='${library.path}' artifact=$artifact"
        )
    }
    val (loadAlignments, relroEnds) = wideHeaders.second.parseElfProgramHeaders()

    val compatible = is16KbElfLayoutCompatible(loadAlignments, relroEnds)
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

val testNativeLibraryValidation by tasks.registering {
    group = "verification"
    description = "Runs deterministic regression fixtures for Android archive and ELF validation."

    doLast {
        fun expectFailure(description: String, action: () -> Unit) {
            if (runCatching(action).exceptionOrNull() !is GradleException) {
                throw GradleException("Expected native validation fixture to fail: $description")
            }
        }

        check(parseAndroidNativeEntryPath("jni/arm64-v8a/libldk_node.so") == ("arm64-v8a" to "libldk_node.so"))
        expectFailure("parent traversal") { parseAndroidNativeEntryPath("jni/../../outside.so") }
        expectFailure("nested native path") { parseAndroidNativeEntryPath("jni/arm64-v8a/nested/lib.so") }
        expectFailure("missing required ABI") {
            requireAndroidNativeEntries(setOf("jni/arm64-v8a/libldk_node.so"))
        }
        expectFailure("duplicate native entry") {
            requireUniqueAndroidNativeEntries(
                listOf(
                    "jni/arm64-v8a/libldk_node.so",
                    "jni/arm64-v8a/libldk_node.so"
                )
            )
        }
        requireUniqueAndroidNativeEntries(androidNativeAbis.map { "jni/$it/libldk_node.so" })
        requireAndroidNativeEntries(androidNativeAbis.map { "jni/$it/libldk_node.so" }.toSet())

        check(".debug_info".findUnstrippedElfSection() == ".debug_")
        check(".zdebug_info".findUnstrippedElfSection() == ".zdebug_")
        check(".symtab".findUnstrippedElfSection() == ".symtab")
        check(".dynsym".findUnstrippedElfSection() == null)

        val compatibleHeaders = """
            LOAD 0x0 0x0 0x0 0x100 0x100 R 0x4000
            LOAD 0x4000 0x4000 0x4000 0x100 0x100 R E 0x4000
            GNU_RELRO 0x7000 0x7000 0x7000 0x1000 0x1000 R 0x1
        """.trimIndent()
        val (loads, relroEnds) = compatibleHeaders.parseElfProgramHeaders()
        check(loads == listOf(0x4000L, 0x4000L))
        check(relroEnds == listOf(0x8000L))
        check(is16KbElfLayoutCompatible(loads, relroEnds))
        check(!is16KbElfLayoutCompatible(listOf(0x1000L), relroEnds))
        check(!is16KbElfLayoutCompatible(loads, listOf(0x8100L)))
        check(!is16KbElfLayoutCompatible(emptyList(), relroEnds))
        check(!is16KbElfLayoutCompatible(loads, emptyList()))
        androidElfIdentities.forEach { (abi, identity) ->
            val fixture = temporaryDir.resolve("$abi.so")
            val header = ByteArray(20)
            header[0] = 0x7f
            header[1] = 'E'.code.toByte()
            header[2] = 'L'.code.toByte()
            header[3] = 'F'.code.toByte()
            header[4] = identity.first.toByte()
            header[5] = 1
            header[18] = (identity.second and 0xff).toByte()
            header[19] = (identity.second shr 8).toByte()
            fixture.writeBytes(header)
            check(fixture.readElfIdentity() == identity)
            requireMatchingAndroidElfIdentity(abi, fixture.readElfIdentity(), fixture, "fixture")
        }
        expectFailure("swapped ABI") {
            requireMatchingAndroidElfIdentity(
                "arm64-v8a",
                androidElfIdentities.getValue("x86_64"),
                temporaryDir.resolve("swapped.so"),
                "fixture"
            )
        }
        expectFailure("unsupported ABI") {
            requireMatchingAndroidElfIdentity(
                "unsupported",
                androidElfIdentities.getValue("arm64-v8a"),
                temporaryDir.resolve("unsupported.so"),
                "fixture"
            )
        }
        expectFailure("truncated ELF header") {
            temporaryDir.resolve("truncated.so").apply { writeBytes(byteArrayOf(0x7f)) }.readElfIdentity()
        }
    }
}

val validateReleaseNativeLibraries by tasks.registering {
    group = "verification"
    description = "Validates source release JNI libraries are stripped and 16 KB page-size compatible."
    dependsOn(testNativeLibraryValidation)

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
                .map { entry ->
                    val (abi, fileName) = try {
                        parseAndroidNativeEntryPath(entry.name)
                    } catch (error: GradleException) {
                        throw GradleException("${error.message} artifact='${aar.path}'")
                    }
                    Triple(entry, abi, fileName)
                }
                .toList()
            if (nativeEntries.isEmpty()) {
                throw GradleException("Android release AAR contains no native libraries: artifact='${aar.path}'")
            }

            val packagedEntryNames = nativeEntries.map { it.first.name }
            try {
                requireUniqueAndroidNativeEntries(packagedEntryNames)
            } catch (error: GradleException) {
                throw GradleException("${error.message} artifact='${aar.path}'")
            }
            val packagedEntries = packagedEntryNames.toSet()
            try {
                requireAndroidNativeEntries(packagedEntries)
            } catch (error: GradleException) {
                throw GradleException("${error.message} artifact='${aar.path}'")
            }

            val extractionRoot = temporaryDir.canonicalFile
            nativeEntries.forEach { (entry, abi, fileName) ->
                val extracted = extractionRoot.resolve("$abi/$fileName").canonicalFile
                if (!extracted.toPath().startsWith(extractionRoot.toPath())) {
                    throw GradleException(
                        "Android release AAR native library escapes the extraction directory: " +
                            "library=${entry.name} artifact='${aar.path}'"
                    )
                }
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
