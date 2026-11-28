package org.mycel.app

import android.content.Context
import android.os.Build
import android.os.VibrationEffect
import android.os.Vibrator
import android.os.VibratorManager

/** Called from Rust (`vibrate`). */
object Haptics {
  @JvmStatic
  @Suppress("DEPRECATION") // VIBRATOR_SERVICE and vibrate(Long) are what minSdk 24 has
  fun vibrate(context: Context, ms: Long) {
    val v = if (Build.VERSION.SDK_INT >= 31) {
      (context.getSystemService(Context.VIBRATOR_MANAGER_SERVICE) as VibratorManager).defaultVibrator
    } else {
      context.getSystemService(Context.VIBRATOR_SERVICE) as Vibrator
    }
    when {
      // The device's own tuned "click": many phones drop a 20-50 ms one-shot as too short to feel.
      Build.VERSION.SDK_INT >= 29 ->
        v.vibrate(VibrationEffect.createPredefined(if (ms >= 50) VibrationEffect.EFFECT_HEAVY_CLICK else VibrationEffect.EFFECT_CLICK))
      Build.VERSION.SDK_INT >= 26 -> v.vibrate(VibrationEffect.createOneShot(maxOf(ms, 40), 255))
      else -> v.vibrate(maxOf(ms, 40))
    }
  }
}
