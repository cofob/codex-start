plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
    id("org.jlleitschuh.gradle.ktlint")
}

ktlint {
    filter {
        exclude("**/codex_start_android.kt")
        exclude { element -> element.file.path.contains("/build/") }
    }
}

tasks.withType<org.jlleitschuh.gradle.ktlint.tasks.BaseKtLintCheckTask>().configureEach {
    exclude("**/codex_start_android.kt")
    exclude { element -> element.file.path.contains("/build/") }
}

val nativeRoot = layout.buildDirectory.dir("native")
val buildNative by tasks.registering(Exec::class) {
    workingDir(rootProject.projectDir.parentFile)
    commandLine("python3", "scripts/build-android-native.py", nativeRoot.get().asFile.absolutePath)
    inputs.files(
        fileTree("../../crates/codex-start-client"),
        fileTree("../../crates/codex-start-remote"),
        fileTree("../../crates/codex-start-transport"),
        fileTree("../../crates/codex-start-android"),
        fileTree("../../vendor"),
        file("../../Cargo.lock"),
        file("../../Cargo.toml"),
        file("../../scripts/build-android-native.py"),
        file("../../assets/public-peers.json"),
    )
    outputs.dir(nativeRoot)
}
tasks.named("runKtlintCheckOverMainSourceSet") { dependsOn(buildNative) }
tasks.named("runKtlintFormatOverMainSourceSet") { dependsOn(buildNative) }

android {
    namespace = "wtf.fob.cs"
    compileSdk = 36
    ndkVersion = "28.2.13676358"
    defaultConfig {
        applicationId = "wtf.fob.cs"
        minSdk = 28
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }
    signingConfigs {
        create("release") {
            System.getenv("ANDROID_KEYSTORE")?.let { storeFile = file(it) }
            storePassword = System.getenv("ANDROID_STORE_PASSWORD")
            keyAlias = System.getenv("ANDROID_KEY_ALIAS")
            keyPassword = System.getenv("ANDROID_KEY_PASSWORD")
        }
    }
    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName("release")
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }
    sourceSets["main"].apply {
        java.srcDir(files(nativeRoot.map { it.dir("kotlin") }).builtBy(buildNative))
        jniLibs.srcDir(nativeRoot.map { it.dir("jniLibs") })
        assets.srcDir("../../protocol/codex")
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
    buildFeatures {
        compose = true
        buildConfig = true
    }
    packaging { resources.excludes += listOf("META-INF/AL2.0", "META-INF/LGPL2.1") }
    testOptions { unitTests.isReturnDefaultValues = true }
    lint {
        abortOnError = true
        checkReleaseBuilds = true
    }
}
tasks.named("preBuild") { dependsOn(buildNative) }
listOf("debug", "release").forEach { variant ->
    val title = variant.replaceFirstChar { it.uppercase() }
    val verifyNative =
        tasks.register<Exec>("verify${title}NativeAlignment") {
            commandLine(
                "python3",
                "../../scripts/check-android-native.py",
                layout.buildDirectory
                    .file("outputs/apk/$variant/app-$variant.apk")
                    .get()
                    .asFile.absolutePath,
            )
        }
    tasks.matching { it.name == "assemble$title" }.configureEach { finalizedBy(verifyNative) }
}
dependencies {
    implementation(platform("androidx.compose:compose-bom:2025.04.01"))
    implementation("androidx.core:core-ktx:1.16.0")
    implementation("androidx.activity:activity-compose:1.10.1")
    implementation("androidx.window:window:1.5.1")
    implementation("androidx.navigation:navigation-compose:2.9.8")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.9.0")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.9.0")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-extended")
    implementation("androidx.compose.ui:ui-tooling-preview")
    debugImplementation("androidx.compose.ui:ui-tooling")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.10.2")
    // 5.17 fixes Android 16 KB RELRO; 5.18.1 also fixes concurrent Structure access.
    implementation("net.java.dev.jna:jna:5.18.1@aar")
    implementation("androidx.graphics:graphics-path:1.1.0")
    val cameraXVersion = "1.5.3"
    implementation("androidx.camera:camera-camera2:$cameraXVersion")
    implementation("androidx.camera:camera-lifecycle:$cameraXVersion")
    implementation("androidx.camera:camera-view:$cameraXVersion")
    // Bundle the model so scanning works offline without Google Play Services.
    implementation("com.google.mlkit:barcode-scanning:17.3.0")
    implementation("io.noties.markwon:core:4.6.2")
    testImplementation("junit:junit:4.13.2")
    testImplementation("org.json:json:20250107")
    androidTestImplementation("androidx.test.ext:junit:1.3.0")
    androidTestImplementation("androidx.test:runner:1.7.0")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.7.0")
    androidTestImplementation(platform("androidx.compose:compose-bom:2025.04.01"))
    androidTestImplementation("androidx.compose.ui:ui-test-junit4")
    debugImplementation("androidx.compose.ui:ui-test-manifest")
}
