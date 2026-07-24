import groovy.json.JsonSlurper
import java.io.ByteArrayOutputStream
import java.io.File
import java.nio.file.Files
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

fun findLlvmTool(name: String): String {
    executableFromPath(name)?.let { return it }

    return listOf("ANDROID_NDK_ROOT", "ANDROID_NDK_HOME", "NDK_HOME")
        .mapNotNull { System.getenv(it) }
        .map { File(it, "toolchains/llvm/prebuilt") }
        .firstNotNullOfOrNull { prebuiltDir ->
            if (!prebuiltDir.isDirectory) return@firstNotNullOfOrNull null

            prebuiltDir
                .walkTopDown()
                .firstOrNull { it.name == name && it.canExecute() }
                ?.absolutePath
        }
        ?: throw GradleException(
            "$name is required to validate the UniFFI Kotlin/native contract"
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

fun Project.runTool(tool: String, vararg args: String): Pair<Int, String> {
    val stdout = ByteArrayOutputStream()
    val stderr = ByteArrayOutputStream()
    val result = exec {
        commandLine(tool, *args)
        standardOutput = stdout
        errorOutput = stderr
        isIgnoreExitValue = true
    }

    return result.exitValue to stdout.toString().ifBlank { stderr.toString() }
}

fun String.parseElfNumber(): Long {
    return if (startsWith("0x")) {
        removePrefix("0x").toLong(16)
    } else {
        toLong()
    }
}

fun Long.toElfHex(): String = "0x${toString(16)}"

data class ElfPageSizeInfo(
    val loadAlignments: List<Long>,
    val relroStart: Long?,
    val relroSize: Long?
) {
    val relroEnd: Long?
        get() = if (relroStart != null && relroSize != null) relroStart + relroSize else null

    fun detectedValues(): String {
        val loads = if (loadAlignments.isEmpty()) {
            "missing"
        } else {
            loadAlignments.joinToString(",") { it.toElfHex() }
        }
        return "LOAD_ALIGNMENTS=[$loads] " +
            "GNU_RELRO_START=${relroStart?.toElfHex() ?: "missing"} " +
            "GNU_RELRO_MEMSZ=${relroSize?.toElfHex() ?: "missing"} " +
            "GNU_RELRO_END=${relroEnd?.toElfHex() ?: "missing"}"
    }
}

fun parseElfPageSizeInfo(headers: String): ElfPageSizeInfo {
    val loadAlignments = mutableListOf<Long>()
    var relroStart: Long? = null
    var relroSize: Long? = null

    headers.lineSequence().forEach { line ->
        val columns = line.trim().split(Regex("""\s+"""))
        when (columns.firstOrNull()) {
            "LOAD" -> columns.lastOrNull()?.let { loadAlignments += it.parseElfNumber() }
            "GNU_RELRO" -> {
                if (columns.size >= 6) {
                    relroStart = columns[2].parseElfNumber()
                    relroSize = columns[5].parseElfNumber()
                }
            }
        }
    }

    return ElfPageSizeInfo(loadAlignments, relroStart, relroSize)
}

fun Project.validateAndroidNativeLibrary(
    readelf: String,
    abi: String,
    lib: File,
    displayPath: String = lib.path
) {
    if (!lib.isFile) {
        throw GradleException("Android native library missing: ABI='$abi' path='$displayPath'")
    }

    val (sectionsExit, sections) = runReadelf(readelf, "-S", lib.absolutePath)
    if (sectionsExit != 0) {
        throw GradleException("Unable to inspect Android native library sections: ABI='$abi' path='$displayPath'")
    }
    if (Regex("""\.debug_""").containsMatchIn(sections)) {
        throw GradleException(
            "Android release native library still contains .debug_* sections: ABI='$abi' path='$displayPath'"
        )
    }

    val wideHeaders = runReadelf(readelf, "-W", "-l", lib.absolutePath)
    val headers = if (wideHeaders.first == 0) {
        wideHeaders.second
    } else {
        val fallbackHeaders = runReadelf(readelf, "-l", lib.absolutePath)
        if (fallbackHeaders.first != 0) {
            throw GradleException(
                "Unable to inspect Android native library headers: ABI='$abi' path='$displayPath'"
            )
        }
        fallbackHeaders.second
    }

    val pageSize = 16_384L
    val info = parseElfPageSizeInfo(headers)
    val relroEnd = info.relroEnd
    val failures = buildList {
        if (info.loadAlignments.isEmpty()) {
            add("PT_LOAD is missing")
        } else if (info.loadAlignments.any { it < pageSize }) {
            add("PT_LOAD alignment is below 0x4000")
        }
        if (relroEnd == null) {
            add("PT_GNU_RELRO is missing")
        } else if (relroEnd % pageSize != 0L) {
            add("PT_GNU_RELRO end is not aligned to 0x4000")
        }
    }

    if (failures.isNotEmpty()) {
        throw GradleException(
            "Android 16 KB ELF validation failed: ABI='$abi' path='$displayPath' " +
                "${info.detectedValues()} failures=${failures.joinToString("; ")}"
        )
    }

    logger.lifecycle(
        "Android 16 KB ELF validation passed: ABI='$abi' path='$displayPath' ${info.detectedValues()}"
    )
}

data class KotlinUniffiIntegrity(
    val contractVersion: Int,
    val apiChecksums: Map<String, Int>
)

fun kotlinUniffiIntegrity(): KotlinUniffiIntegrity {
    val generatedBinding = layout.projectDirectory
        .file("src/main/kotlin/com/synonym/paykit/paykit.android.kt")
        .asFile
    if (!generatedBinding.isFile) {
        throw GradleException(
            "Generated Kotlin binding is missing at '${generatedBinding.path}'"
        )
    }

    val source = generatedBinding.readText()
    val contractMatch = Regex(
        """(?m)^\s*val bindings_?[Cc]ontract_?[Vv]ersion = (\d+).*$"""
    ).find(source)
        ?: throw GradleException(
            "Unable to extract the UniFFI contract version from '${generatedBinding.path}'"
        )
    val checksumMatches = Regex(
        """if \((?:lib\.)?(uniffi_paykit_checksum_[A-Za-z0-9_]+)\(\) != (\d+)\.toShort\(\)\)"""
    ).findAll(source).toList()
    if (checksumMatches.isEmpty()) {
        throw GradleException(
            "Unable to extract UniFFI API checksums from '${generatedBinding.path}'"
        )
    }
    val duplicateChecksums = checksumMatches
        .groupBy { it.groupValues[1] }
        .filterValues { it.size > 1 }
        .keys
    if (duplicateChecksums.isNotEmpty()) {
        throw GradleException(
            "Duplicate UniFFI API checksum expectations in '${generatedBinding.path}': " +
                duplicateChecksums.sorted().joinToString()
        )
    }

    return KotlinUniffiIntegrity(
        contractVersion = contractMatch.groupValues[1].toInt(),
        apiChecksums = checksumMatches.associate {
            it.groupValues[1] to it.groupValues[2].toInt()
        }
    )
}

data class NativeSymbol(
    val address: Long,
    val size: Long
)

fun Project.nativeDynamicSymbols(
    nm: String,
    abi: String,
    lib: File,
    displayPath: String
): Map<String, NativeSymbol> {
    val (nmExit, symbols) = runTool(nm, "-D", "-S", lib.absolutePath)
    if (nmExit != 0) {
        throw GradleException(
            "Unable to inspect native symbols: ABI='$abi' path='$displayPath'"
        )
    }

    return symbols.lineSequence()
        .map { it.trim().split(Regex("""\s+""")) }
        .filter { it.size >= 4 }
        .associate { fields ->
            fields.last() to NativeSymbol(
                address = fields[0].toLong(16),
                size = fields[1].toLong(16)
            )
        }
}

fun Project.nativeConstantValue(
    objdump: String,
    abi: String,
    lib: File,
    displayPath: String,
    symbols: Map<String, NativeSymbol>,
    symbolName: String
): Int {
    val symbol = symbols[symbolName]
        ?: throw GradleException(
            "UniFFI integrity symbol is missing: ABI='$abi' symbol='$symbolName' path='$displayPath'"
        )

    val objdumpArgs = buildList {
        add("-d")
        add("--no-show-raw-insn")
        add("--start-address=${symbol.address}")
        add("--stop-address=${symbol.address + symbol.size}")
        if (abi == "armeabi-v7a") {
            add("--triple=thumbv7-none-linux-android")
        }
        add(lib.absolutePath)
    }
    val (objdumpExit, disassembly) = runTool(objdump, *objdumpArgs.toTypedArray())
    if (objdumpExit != 0) {
        throw GradleException(
            "Unable to disassemble UniFFI integrity symbol: " +
                "ABI='$abi' symbol='$symbolName' path='$displayPath'"
        )
    }

    val immediate = sequenceOf(
        Regex("""\bmovs?\s+r0,\s*#0x([0-9a-fA-F]+)"""),
        Regex("""\bmovw\s+r0,\s*#0x([0-9a-fA-F]+)"""),
        Regex("""\bmov\.w\s+r0,\s*#0x([0-9a-fA-F]+)"""),
        Regex("""\bmov\s+w0,\s*#0x([0-9a-fA-F]+)"""),
        Regex("""\bmov[wl]\s+\$0x([0-9a-fA-F]+),\s*%e?ax""")
    ).mapNotNull { it.find(disassembly)?.groupValues?.get(1) }
        .firstOrNull()
        ?: throw GradleException(
            "Unable to decode UniFFI integrity symbol: " +
                "ABI='$abi' symbol='$symbolName' path='$displayPath'\n$disassembly"
        )

    return immediate.toInt(16)
}

fun Project.validateUniffiIntegrity(
    nm: String,
    objdump: String,
    expected: KotlinUniffiIntegrity,
    abi: String,
    lib: File,
    displayPath: String
) {
    val symbols = nativeDynamicSymbols(nm, abi, lib, displayPath)
    val nativeChecksumSymbols = symbols.keys.filter {
        it.startsWith("uniffi_paykit_checksum_")
    }.toSet()
    val expectedChecksumSymbols = expected.apiChecksums.keys
    if (nativeChecksumSymbols != expectedChecksumSymbols) {
        val missing = expectedChecksumSymbols - nativeChecksumSymbols
        val extra = nativeChecksumSymbols - expectedChecksumSymbols
        throw GradleException(
            "UniFFI API checksum symbol set mismatch: ABI='$abi' path='$displayPath' " +
                "missing=${missing.sorted()} extra=${extra.sorted()}"
        )
    }

    val nativeContract = nativeConstantValue(
        objdump,
        abi,
        lib,
        displayPath,
        symbols,
        "ffi_paykit_uniffi_contract_version"
    )
    if (nativeContract != expected.contractVersion) {
        throw GradleException(
            "UniFFI Kotlin/native contract mismatch: ABI='$abi' " +
                "Kotlin=${expected.contractVersion} native=$nativeContract path='$displayPath'"
        )
    }

    expected.apiChecksums.toSortedMap().forEach { (symbolName, expectedChecksum) ->
        val nativeChecksum = nativeConstantValue(
            objdump,
            abi,
            lib,
            displayPath,
            symbols,
            symbolName
        )
        if (nativeChecksum != expectedChecksum) {
            throw GradleException(
                "UniFFI API checksum mismatch: ABI='$abi' symbol='$symbolName' " +
                    "Kotlin=$expectedChecksum native=$nativeChecksum path='$displayPath'"
            )
        }
    }

    logger.lifecycle(
        "UniFFI Kotlin/native integrity validation passed: ABI='$abi' " +
            "contract=${expected.contractVersion} checksums=${expected.apiChecksums.size} " +
            "path='$displayPath'"
    )
}

val validateReleaseNativeLibraries by tasks.registering {
    group = "verification"
    description = "Validates source release JNI libraries for 16 KB and UniFFI integrity."

    doLast {
        val readelf = findReadelf()
        val nm = findLlvmTool("llvm-nm")
        val objdump = findLlvmTool("llvm-objdump")
        val kotlinIntegrity = kotlinUniffiIntegrity()

        androidNativeAbis.forEach { abi ->
            val lib = layout.projectDirectory.file("src/main/jniLibs/$abi/libpaykit.so").asFile
            validateAndroidNativeLibrary(readelf, abi, lib)
            validateUniffiIntegrity(
                nm,
                objdump,
                kotlinIntegrity,
                abi,
                lib,
                lib.path
            )
        }
    }
}

val validateReleaseAarNativeLibraries by tasks.registering {
    group = "verification"
    description = "Validates every final-AAR native library for 16 KB and UniFFI integrity."
    dependsOn("bundleReleaseAar")

    doLast {
        val readelf = findReadelf()
        val nm = findLlvmTool("llvm-nm")
        val objdump = findLlvmTool("llvm-objdump")
        val kotlinIntegrity = kotlinUniffiIntegrity()
        logger.lifecycle(
            "UniFFI Kotlin integrity manifest: contract=${kotlinIntegrity.contractVersion} " +
                "checksums=${kotlinIntegrity.apiChecksums.size}"
        )
        val aar = layout.buildDirectory.file("outputs/aar/lib-release.aar").get().asFile
        if (!aar.isFile) {
            throw GradleException("Android release AAR missing at '${aar.path}'")
        }

        val tempDir = Files.createTempDirectory("paykit-release-aar-").toFile()
        try {
            ZipFile(aar).use { zip ->
                androidNativeAbis.forEach { abi ->
                    val entryPath = "jni/$abi/libpaykit.so"
                    val entry = zip.getEntry(entryPath)
                        ?: throw GradleException(
                            "Android release AAR native library missing: ABI='$abi' path='$entryPath' AAR='${aar.path}'"
                        )
                    val extracted = File(tempDir, entryPath)
                    extracted.parentFile.mkdirs()
                    zip.getInputStream(entry).use { input ->
                        extracted.outputStream().use { output -> input.copyTo(output) }
                    }
                    validateAndroidNativeLibrary(readelf, abi, extracted, "${aar.path}!/$entryPath")
                    validateUniffiIntegrity(
                        nm,
                        objdump,
                        kotlinIntegrity,
                        abi,
                        extracted,
                        "${aar.path}!/$entryPath"
                    )
                }
            }
        } finally {
            tempDir.deleteRecursively()
        }
    }
}

tasks.matching { it.name == "bundleReleaseAar" }.configureEach {
    dependsOn(validateReleaseNativeLibraries)
}

tasks.matching { it.name == "build" || it.name == "assembleRelease" || it.name.startsWith("publish") }
    .configureEach {
        dependsOn(validateReleaseAarNativeLibraries)
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
