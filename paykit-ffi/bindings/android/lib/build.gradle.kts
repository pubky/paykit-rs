import groovy.json.JsonSlurper
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

afterEvaluate {
    publishing {
        publications {
            create<MavenPublication>("maven") {
                val mavenArtifactId = "paykit-android"
                groupId = providers.gradleProperty("group").orNull ?: "com.synonym"
                artifactId = mavenArtifactId
                version = providers.gradleProperty("version").orNull ?: "0.0.0"

                from(components["release"])
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
