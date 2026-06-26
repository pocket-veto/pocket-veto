// Top-level build file. Plugins are declared here with `apply false` and
// applied in the module-level build files via the plugins DSL. This is the
// modern Gradle convention and keeps per-module versions explicit.
plugins {
    id("com.android.application") version "9.2.1" apply false
    id("org.jetbrains.kotlin.android") version "2.4.0" apply false
    id("org.jetbrains.kotlin.plugin.serialization") version "2.4.0" apply false
    id("org.jetbrains.kotlin.plugin.compose") version "2.4.0" apply false
}
