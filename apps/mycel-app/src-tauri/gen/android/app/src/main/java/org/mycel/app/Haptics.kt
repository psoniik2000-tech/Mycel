package org.mycel.app

import android.content.Context
import android.media.AudioAttributes
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
    // Alarm usage: touch-class haptics are dropped by the "touch feedback" setting and by many
    // vendor ROMs; one firm one-shot, long enough to feel on any motor.
    val alarm = AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_ALARM).build()
    if (Build.VERSION.SDK_INT >= 26) v.vibrate(VibrationEffect.createOneShot(maxOf(ms, 50), 255), alarm)
    else v.vibrate(maxOf(ms, 50), alarm)
  }
}
