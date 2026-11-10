import java.util.Properties

plugins {
  alias(libs.plugins.android.application)
  alias(libs.plugins.compose.compiler)
}

// Release signing material is never committed. Two sources, in order:
//   1. CI: ANDROID_KEYSTORE_FILE + ANDROID_KEYSTORE_PASSWORD/KEY_ALIAS/KEY_PASSWORD
//      (the workflow decodes ANDROID_KEYSTORE_BASE64 into a temp file);
//   2. local: android/keystore/release.keystore + android/keystore/keystore.properties
//      (git-ignored, generated per checkout).
// If neither exists, release falls back to the debug key so the variant still
// produces an installable APK.
val signingPropsFile = rootProject.file("keystore/keystore.properties")
val signingProps = Properties().apply {
  if (signingPropsFile.exists()) signingPropsFile.inputStream().use { load(it) }
}
val envKeystorePath: String? = System.getenv("ANDROID_KEYSTORE_FILE")
val localKeystoreFile: File? = signingProps.getProperty("storeFile")?.let { name ->
  val candidate = rootProject.file("keystore/$name")
  if (candidate.exists()) candidate else file(name)
}
val signingKeystoreFile: File? = when {
  !envKeystorePath.isNullOrBlank() -> file(envKeystorePath)
  localKeystoreFile?.exists() == true -> localKeystoreFile
  else -> null
}
val releaseSigningReady = signingKeystoreFile?.exists() == true

if (!envKeystorePath.isNullOrBlank() && !releaseSigningReady) {
  throw GradleException("ANDROID_KEYSTORE_FILE 指向的密钥库不存在：$envKeystorePath")
}

android {
  namespace = "com.example.bluetoothinputbridge"
  compileSdk = 36
  defaultConfig {
    applicationId = "com.example.bluetoothinputbridge"
    minSdk = 26
    targetSdk = 36
    versionCode = 1
    versionName = "0.2.1"
  }

  signingConfigs {
    if (releaseSigningReady) {
      create("release") {
        storeFile = signingKeystoreFile!!.absoluteFile
        storePassword = System.getenv("ANDROID_KEYSTORE_PASSWORD")
          ?: signingProps.getProperty("storePassword")
        keyAlias = System.getenv("ANDROID_KEY_ALIAS")
          ?: signingProps.getProperty("keyAlias")
        keyPassword = System.getenv("ANDROID_KEY_PASSWORD")
          ?: signingProps.getProperty("keyPassword")
      }
    }
  }

  buildTypes {
    release {
      isMinifyEnabled = false
      // Without a configured keystore (local builds, fork PRs) fall back to the
      // debug key so the release variant still produces an installable APK.
      signingConfig = if (releaseSigningReady) {
        signingConfigs.getByName("release")
      } else {
        logger.warn("未找到 release 签名材料（android/keystore 或 ANDROID_KEYSTORE_FILE），release 变体将使用 debug 签名")
        signingConfigs.getByName("debug")
      }
      proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
    }
  }
  compileOptions {
    sourceCompatibility = JavaVersion.VERSION_21
    targetCompatibility = JavaVersion.VERSION_21
  }
  buildFeatures {
    compose = true
    aidl = false
    buildConfig = false
    shaders = false
  }

  packaging {
    resources {
      excludes += "/META-INF/{AL2.0,LGPL2.1}"
    }
  }
}

kotlin {
  jvmToolchain(21)
}

dependencies {
  val composeBom = platform(libs.androidx.compose.bom)
  implementation(composeBom)

  // Core Android dependencies
  implementation(libs.androidx.core.ktx)
  implementation(libs.androidx.lifecycle.runtime.ktx)
  implementation(libs.androidx.activity.compose)

  // Arch Components
  implementation(libs.androidx.lifecycle.runtime.compose)

  // Compose
  implementation(libs.androidx.compose.ui)
  implementation(libs.androidx.compose.ui.tooling.preview)
  implementation(libs.androidx.compose.material3)
  // Tooling
  debugImplementation(libs.androidx.compose.ui.tooling)
  implementation(libs.androidx.lifecycle.viewmodel.compose)
}
