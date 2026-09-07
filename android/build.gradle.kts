plugins {
    id("com.android.application") version "8.10.1" apply false
    id("org.jetbrains.kotlin.android") version "2.1.20" apply false
    id("org.jetbrains.kotlin.plugin.compose") version "2.1.20" apply false
    id("org.jlleitschuh.gradle.ktlint") version "14.2.0"
}

ktlint {
    kotlinScriptAdditionalPaths {
        include(fileTree(".") { include("settings.gradle.kts") })
    }
}
