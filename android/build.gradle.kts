// Versions are pinned rather than dynamic so a rebuild months from now produces
// the same APK. Android Studio's upgrade assistant will offer newer ones; let it.
// AGP 9 has built-in Kotlin support, so `org.jetbrains.kotlin.android` is gone —
// applying it is now an error, not merely redundant. The Compose compiler plugin
// is still applied separately.
plugins {
    id("com.android.application") version "9.3.1" apply false
    id("org.jetbrains.kotlin.plugin.compose") version "2.4.10" apply false
}
