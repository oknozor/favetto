// Custom Android entry point for the favetto client.
//
// The soft keyboard must render on its own layer above the WebView. Without
// this, focusing a text field resizes the window and pushes the fixed bottom
// navigation, dialogs and toasts up (and recentres them in the shrunken
// viewport). `SOFT_INPUT_ADJUST_NOTHING` keeps the window metrics fixed while
// the IME is shown; the client's `interactive-widget=overlays-content`
// viewport hint lets the keyboard float over the page instead.
//
// `dx` reads this file through `android_main_activity` in `web/Dioxus.toml`
// and writes it into the generated Gradle project in place of the default
// `MainActivity.kt`.
package dev.dioxus.main

import android.os.Bundle
import android.view.WindowManager

typealias BuildConfig = dev.favetto.app.BuildConfig

class MainActivity : WryActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_ADJUST_NOTHING)
    }
}
