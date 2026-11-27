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
    // Restarted by the system after its process was killed (no intent): start the app in the
    // background, so its UI reopens the last space with the remembered password (Settings → Вход).
    // MainActivity sends itself to the back for this extra.
    if (intent == null) {
      startActivity(Intent(this, MainActivity::class.java)
        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        .putExtra(MainActivity.BACKGROUND, true))
    }
    return START_STICKY
  }
}
