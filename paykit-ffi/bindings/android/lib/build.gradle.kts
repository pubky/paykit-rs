import org.gradle.api.publish.maven.tasks.PublishToMavenRepository

plugins {
    id("com.android.library") version "9.3.0"
    id("maven-publish")
}

group = "com.synonym"
version = providers.gradleProperty("version").get()
val workspacePackage = rootProject.file("../../../Cargo.toml").readText()
    .substringAfter("[workspace.package]\n", "").substringBefore("\n[")
check(workspacePackage.lineSequence().contains("version = \"$version\"")) {
    "Android version $version must match the workspace package version in Cargo.toml"
}

android {
    namespace = "com.synonym.paykit"
    compileSdk = 36
    defaultConfig {
        minSdk = 28
        consumerProguardFiles("consumer-rules.pro")
    }
    sourceSets {
        getByName("main").kotlin.directories.add("src/main/generated/kotlin")
    }
    buildTypes.getByName("release") {
        proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
    }
    publishing.singleVariant("release") {
        withSourcesJar()
    }
}

dependencies {
    api("net.java.dev.jna:jna:5.17.0@aar")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.10.2")
}

val checkBindings = tasks.register("checkBindings") {
    val main = layout.projectDirectory.dir("src/main")
    doLast {
        val libraries = listOf("arm64-v8a", "armeabi-v7a", "x86_64").map { "jniLibs/$it/libpaykit.so" }
        for (path in libraries + "generated/kotlin/com/synonym/paykit/paykit.kt") {
            check(main.file(path).asFile.length() > 0) { "Missing $path. Run paykit-ffi/build_android.sh." }
        }
    }
}

tasks.named("preBuild") { dependsOn(checkBindings) }

tasks.withType<PublishToMavenRepository>().configureEach {
    doFirst {
        for (variable in listOf("GITHUB_ACTOR", "GITHUB_TOKEN")) {
            check(!System.getenv(variable).isNullOrBlank()) { "Publishing requires $variable" }
        }
    }
}

afterEvaluate {
    publishing.publications.create<MavenPublication>("maven") {
        artifactId = "paykit-android"
        from(components["release"])
    }
}

publishing {
    repositories {
        maven {
            name = "GitHubPackages"
            url = uri("https://maven.pkg.github.com/pubky/paykit-rs")
            credentials {
                username = providers.environmentVariable("GITHUB_ACTOR").orNull
                password = providers.environmentVariable("GITHUB_TOKEN").orNull
            }
        }
    }
}
