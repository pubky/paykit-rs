package com.synonym.paykit.rnexample

import android.os.Bundle
import com.facebook.react.ReactActivity
import com.facebook.react.ReactActivityDelegate
import com.facebook.react.defaults.DefaultNewArchitectureEntryPoint.fabricEnabled
import com.facebook.react.defaults.DefaultReactActivityDelegate

class MainActivity : ReactActivity() {

  override fun getMainComponentName(): String = "PaykitExample"

  /**
   * Passes the smoke harness launch options to JS as initial props, for example
   * `adb shell am start -n com.synonym.paykit.rnexample/.MainActivity --es paykitMode local --es paykitAutorun true`.
   * Only string extras are read. The keys match LAUNCH_KEYS in src/launchConfig.ts.
   */
  override fun createReactActivityDelegate(): ReactActivityDelegate =
      object : DefaultReactActivityDelegate(this, mainComponentName, fabricEnabled) {
        override fun getLaunchOptions(): Bundle {
          val extras = intent?.extras
          return Bundle().apply {
            for (key in LAUNCH_KEYS) {
              extras?.getString(key)?.let { putString(key, it) }
            }
          }
        }
      }

  private companion object {
    val LAUNCH_KEYS =
        listOf(
            "paykitMode",
            "paykitAutorun",
            "paykitHomeserver",
            "paykitLocalHost",
            "paykitSignupTokenA",
            "paykitSignupTokenB",
        )
  }
}
