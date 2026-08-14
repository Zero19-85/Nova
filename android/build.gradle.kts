// Versions are pinned rather than dynamic so a rebuild months from now produces
// the same APK. Android Studio's upgrade assistant will offer newer ones; let it.
plugins {
    id("com.android.application") version "8.7.2" apply false
    id("org.jetbrains.kotlin.android") version "2.0.21" apply false
    id("org.jetbrains.kotlin.plugin.compose") version "2.0.21" apply false
}
