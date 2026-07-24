import groovy.json.JsonSlurper
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
    namespace = "com.synonym.paykit"
    compileSdk = 34

    defaultConfig {
        minSdk = 21
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        consumerProguardFiles("consumer-rules.pro")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }

    kotlinOptions {
        jvmTarget = "1.8"
    }

    sourceSets {
        getByName("main") {
            java.srcDir("src/main/kotlin-manual")
        }
    }

    buildTypes {
        getByName("release") {
            isMinifyEnabled = false
            proguardFiles(file("proguard-android-optimize.txt"), file("proguard-rules.pro"))
        }
    }

    publishing {
        singleVariant("release") {
            withSourcesJar()
            withJavadocJar()
        }
    }
}

val rustlsPlatformVerifierClassesJar =
    layout.buildDirectory.file("rustls-platform-verifier/rustls-platform-verifier.jar")

val extractRustlsPlatformVerifierClasses by tasks.registering {
    val outputFile = rustlsPlatformVerifierClassesJar

    outputs.file(outputFile)

    doLast {
        val metadataText = providers.exec {
            workingDir = rootProject.file("../..")
            commandLine(
                "cargo",
                "metadata",
                "--format-version",
                "1",
                "--filter-platform",
                "aarch64-linux-android",
                "--manifest-path",
                "Cargo.toml"
            )
        }.standardOutput.asText.get()

        @Suppress("UNCHECKED_CAST")
        val metadata = JsonSlurper().parseText(metadataText) as Map<String, Any>

        @Suppress("UNCHECKED_CAST")
        val packages = metadata["packages"] as List<Map<String, Any>>
        val rustlsAndroidPackage = packages.first {
            it["name"] == "rustls-platform-verifier-android"
        }
        val manifestPath = file(rustlsAndroidPackage["manifest_path"] as String)
        val version = rustlsAndroidPackage["version"] as String
        val aarPath = File(
            manifestPath.parentFile,
            "maven/rustls/rustls-platform-verifier/$version/rustls-platform-verifier-$version.aar"
        )

        require(aarPath.isFile) {
            "rustls-platform-verifier Android AAR not found at $aarPath"
        }

        val target = outputFile.get().asFile
        target.parentFile.mkdirs()
        ZipFile(aarPath).use { aar ->
            val classesJar = aar.getEntry("classes.jar")
                ?: error("classes.jar missing from $aarPath")
            aar.getInputStream(classesJar).use { input ->
                target.outputStream().use { output ->
                    input.copyTo(output)
                }
            }
        }
    }
}

tasks.named("preBuild") {
    dependsOn(extractRustlsPlatformVerifierClasses)
}

dependencies {
    implementation("net.java.dev.jna:jna:5.17.0@aar")
    implementation("org.jetbrains.kotlin:kotlin-stdlib-jdk8")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.7.3")
    implementation("androidx.appcompat:appcompat:1.6.1")
    implementation("androidx.core:core-ktx:1.12.0")
    implementation("org.jetbrains.kotlinx:atomicfu:0.23.1")
    implementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.6.0")
    implementation(files(rustlsPlatformVerifierClassesJar))
    api("org.slf4j:slf4j-api:1.7.36")
}

val androidNativeAbis = listOf("armeabi-v7a", "arm64-v8a", "x86", "x86_64")

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

fun Project.gnuBuildId(readelf: String, lib: File): String {
    val (notesExit, notes) = runReadelf(readelf, "-n", lib.absolutePath)
    val buildId = Regex("""(?s)NT_GNU_BUILD_ID.*?Build ID:\s*([0-9a-fA-F]+)""")
        .find(notes)
        ?.groupValues
        ?.get(1)
    if (notesExit != 0 || buildId.isNullOrEmpty()) {
        throw GradleException("Android native library has no NT_GNU_BUILD_ID: '${lib.path}'")
    }

    return buildId
}

fun String.parseElfAlignment(): Long {
    return if (startsWith("0x")) {
        removePrefix("0x").toLong(16)
    } else {
        toLong()
    }
}

val validateReleaseNativeLibraries by tasks.registering {
    group = "verification"
    description = "Validates release JNI libraries are stripped, 16 KB aligned, and carry GNU build IDs."

    doLast {
        val readelf = findReadelf()
        val loadAlignmentRegex = Regex("""^\s*LOAD\s+.*\s+(0x[0-9a-fA-F]+|\d+)\s*$""")
        androidNativeAbis.forEach { abi ->
            val lib = layout.projectDirectory.file("src/main/jniLibs/$abi/libpaykit.so").asFile
            if (!lib.isFile) {
                throw GradleException("Android native library missing at '${lib.path}'")
            }

            val (sectionsExit, sections) = runReadelf(readelf, "-S", lib.absolutePath)
            if (sectionsExit != 0) {
                throw GradleException("Unable to inspect Android native library sections: '${lib.path}'")
            }
            if (Regex("""\.debug_""").containsMatchIn(sections)) {
                throw GradleException("Android release native library still contains .debug_* sections: '${lib.path}'")
            }

            gnuBuildId(readelf, lib)

            val wideHeaders = runReadelf(readelf, "-W", "-l", lib.absolutePath)
            val headers = if (wideHeaders.first == 0) {
                wideHeaders.second
            } else {
                val fallbackHeaders = runReadelf(readelf, "-l", lib.absolutePath)
                if (fallbackHeaders.first != 0) {
                    throw GradleException("Unable to inspect Android native library headers: '${lib.path}'")
                }
                fallbackHeaders.second
            }

            val alignments = headers
                .lineSequence()
                .mapNotNull { loadAlignmentRegex.matchEntire(it)?.groupValues?.get(1)?.parseElfAlignment() }
                .toList()

            if (alignments.isEmpty() || alignments.any { it < 16_384 }) {
                throw GradleException("Android native library is not 16 KB page-size aligned: '${lib.path}'")
            }
        }
    }
}

val validatePublishedNativeArtifacts by tasks.registering {
    group = "verification"
    description = "Validates final AAR and full-DWARF symbol build IDs match for every Android ABI."
    dependsOn("bundleReleaseAar", validateReleaseNativeLibraries)

    doLast {
        val readelf = findReadelf()
        val aar = layout.buildDirectory.dir("outputs/aar").get().asFile
            .listFiles()
            ?.singleOrNull { it.name.endsWith("-release.aar") }
            ?: throw GradleException("Exactly one release AAR is required for native build-ID validation")
        val symbolArchive = rootProject.layout.projectDirectory.file("native-debug-symbols.zip").asFile
        if (!symbolArchive.isFile) {
            throw GradleException("Native debug symbol archive missing at '${symbolArchive.path}'")
        }

        val validationDir = layout.buildDirectory.dir("tmp/validatePublishedNativeArtifacts").get().asFile
        validationDir.deleteRecursively()
        validationDir.mkdirs()

        fun extract(zip: ZipFile, entryName: String, output: File): File {
            val entry = zip.getEntry(entryName)
                ?: throw GradleException("Native artifact entry missing: '$entryName'")
            output.parentFile.mkdirs()
            zip.getInputStream(entry).use { input ->
                output.outputStream().use { input.copyTo(it) }
            }
            return output
        }

        ZipFile(aar).use { aarZip ->
            ZipFile(symbolArchive).use { symbolZip ->
                androidNativeAbis.forEach { abi ->
                    val packaged = extract(
                        aarZip,
                        "jni/$abi/libpaykit.so",
                        File(validationDir, "aar/$abi/libpaykit.so")
                    )
                    val symbols = extract(
                        symbolZip,
                        "$abi/libpaykit.so",
                        File(validationDir, "symbols/$abi/libpaykit.so")
                    )
                    val (sectionsExit, sections) = runReadelf(readelf, "-S", symbols.absolutePath)
                    if (sectionsExit != 0 || !sections.contains(".debug_info")) {
                        throw GradleException("Full DWARF .debug_info missing for '$abi/libpaykit.so'")
                    }

                    val packagedBuildId = gnuBuildId(readelf, packaged)
                    val symbolBuildId = gnuBuildId(readelf, symbols)
                    if (packagedBuildId != symbolBuildId) {
                        throw GradleException(
                            "Native build ID mismatch for '$abi/libpaykit.so': " +
                                "aar=$packagedBuildId symbols=$symbolBuildId"
                        )
                    }
                }
            }
        }
    }
}

tasks.matching { it.name == "bundleReleaseAar" }.configureEach {
    dependsOn(validateReleaseNativeLibraries)
}

tasks.matching { it.name == "build" || it.name == "check" || it.name.startsWith("publish") }.configureEach {
    dependsOn(validatePublishedNativeArtifacts)
}

afterEvaluate {
    publishing {
        publications {
            create<MavenPublication>("maven") {
                val mavenArtifactId = "paykit-android"
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
                    description.set("Paykit Android bindings.")
                    url.set("https://github.com/pubky/paykit-rs")
                    licenses {
                        license {
                            name.set("MIT")
                            url.set("https://github.com/pubky/paykit-rs/blob/master/LICENSE")
                        }
                    }
                    developers {
                        developer {
                            id.set("pubky")
                            name.set("Pubky")
                            email.set("noreply@pubky.org")
                        }
                    }
                }
            }
        }
        repositories {
            maven {
                val repo = System.getenv("GITHUB_REPO")
                    ?: providers.gradleProperty("gpr.repo").orNull
                    ?: "pubky/paykit-rs"
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

ktlint {
    filter {
        exclude { fileTreeElement ->
            fileTreeElement.file.toPath().startsWith(project.layout.buildDirectory.asFile.get().toPath())
        }
        exclude { fileTreeElement ->
            fileTreeElement.file.name in setOf("paykit.android.kt", "paykit.common.kt")
        }
    }
}
