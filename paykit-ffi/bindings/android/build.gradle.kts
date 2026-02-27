buildscript {
    repositories {
        google()
        mavenCentral()
    }
    dependencies {
        classpath("com.android.tools.build:gradle:8.5.2")
    }
}

plugins {
    kotlin("android") version "1.9.20" apply false
    kotlin("plugin.serialization") version "1.9.20" apply false
}

group = providers.gradleProperty("group").orNull ?: "com.synonym"
version = providers.gradleProperty("version").orNull ?: "0.0.0"
