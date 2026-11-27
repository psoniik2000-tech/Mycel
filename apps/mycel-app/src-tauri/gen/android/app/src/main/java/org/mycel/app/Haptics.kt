package org.mycel.app

import android.content.Context
import android.os.Build
import android.os.VibrationEffect
import android.os.Vibrator

/** Called from Rust (`vibrate`). */
object Haptics {
  @JvmStatic
  @Suppress("DEPRECATION") // VIBRATOR_SERVICE and vibrate(Long) are what minSdk 24 has
  fun vibrate(context: Context, ms: Long) {
    val v = context.getSystemService(Context.VIBRATOR_SERVICE) as Vibrator
    if (Build.VERSION.SDK_INT >= 26) v.vibrate(VibrationEffect.createOneShot(ms, VibrationEffect.DEFAULT_AMPLITUDE))
    else v.vibrate(ms)
  }
}
