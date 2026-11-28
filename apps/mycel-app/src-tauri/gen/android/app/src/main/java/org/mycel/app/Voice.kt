package org.mycel.app

import android.content.Context
import android.media.MediaRecorder
import android.os.Build
import java.io.File

/**
 * Voice notes by the system recorder, called from Rust (`voice_*`): WebView getUserMedia takes
 * up to a second to open the mic, MediaRecorder starts in ~0.1 s (as Telegram does).
 */
object Voice {
  private var rec: MediaRecorder? = null
  private var file: File? = null

  /** Starts recording; returns the mime type. Throws without the mic permission. */
  @JvmStatic
  @Suppress("DEPRECATION") // MediaRecorder() is what API < 31 has
  fun start(context: Context): String {
    stop()
    val ogg = Build.VERSION.SDK_INT >= 29 // Opus in Ogg from API 29, AAC in MP4 before
    val f = File(context.cacheDir, if (ogg) "voice.ogg" else "voice.m4a")
    val r = if (Build.VERSION.SDK_INT >= 31) MediaRecorder(context) else MediaRecorder()
    try {
      r.setAudioSource(MediaRecorder.AudioSource.MIC)
      r.setOutputFormat(if (ogg) MediaRecorder.OutputFormat.OGG else MediaRecorder.OutputFormat.MPEG_4)
      r.setAudioEncoder(if (ogg) MediaRecorder.AudioEncoder.OPUS else MediaRecorder.AudioEncoder.AAC)
      r.setAudioChannels(1)
      r.setAudioSamplingRate(48000)
      r.setAudioEncodingBitRate(if (ogg) 24000 else 32000)
      r.setOutputFile(f.absolutePath)
      r.prepare()
      r.start()
    } catch (e: Exception) {
      r.release()
      throw e
    }
    rec = r
    file = f
    return if (ogg) "audio/ogg;codecs=opus" else "audio/mp4"
  }

  /** Stops; returns the file path, or "" when nothing usable was recorded. */
  @JvmStatic
  fun stop(): String {
    val r = rec ?: return ""
    rec = null
    val ok = try {
      r.stop()
      true
    } catch (e: RuntimeException) {
      false // stopped right after start: no data
    }
    r.release()
    return if (ok) file?.absolutePath ?: "" else ""
  }

  /** Peak amplitude since the last call, 0..32767 (the halo around the button). */
  @JvmStatic
  fun level(): Int = try {
    rec?.maxAmplitude ?: 0
  } catch (e: Exception) {
    0
  }
}
