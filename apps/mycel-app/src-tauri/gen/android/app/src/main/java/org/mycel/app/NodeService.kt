package org.mycel.app

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat

/** Keeps the process (and the Rust node in it) alive while the app is in the background (DESIGN §7.5). */
class NodeService : Service() {
  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    if (Build.VERSION.SDK_INT >= 26) {
      getSystemService(NotificationManager::class.java).createNotificationChannel(
        NotificationChannel("node", "Связь в фоне", NotificationManager.IMPORTANCE_MIN))
    }
    val open = PendingIntent.getActivity(
      this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE)
    val n = NotificationCompat.Builder(this, "node")
      .setSmallIcon(R.mipmap.ic_launcher)
      .setContentTitle("Mycel на связи")
      .setContentText("Получает сообщения в фоне")
      .setOngoing(true)
      .setContentIntent(open)
      .build()
    val type = if (Build.VERSION.SDK_INT >= 34) ServiceInfo.FOREGROUND_SERVICE_TYPE_REMOTE_MESSAGING else 0
    ServiceCompat.startForeground(this, 1, n, type)
    // The node needs the password after a restart, so a killed process stays dead until the app is opened.
    return START_NOT_STICKY
  }
}
